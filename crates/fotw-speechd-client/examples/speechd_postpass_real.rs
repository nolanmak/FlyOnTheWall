//! The #246 real-model smoke: the FluidAudio helper's `diarizer` engine and
//! `parakeet-unified` word timings. Separate from `ci` on purpose; run it with
//! `just speechd-postpass-real`.
//!
//! ```text
//! speechd_postpass_real --helper <fotw-speechd> --models-root <dir>
//!     --voice <a.wav> --voice <b.wav> [--offset-s <s>] [--seconds <s>]
//!     [--parakeet-from <dir>] [--diarizer-from <dir>]
//! ```
//!
//! The voices are 16 kHz mono s16 WAVs of different speakers (for example two
//! AMI headset channels of one meeting). The smoke mixes `--seconds` of them
//! from `--offset-s`, then:
//!
//! 1. **Setup**, the only phase that may download: installs `diarizer` and
//!    `parakeet-unified` (imported with `--…-from` if given) when missing.
//! 2. Runs one diarization stream over the mix, paced by its window
//!    acknowledgements, and requires two to four speakers, a label that
//!    returns after another speaker's turn, and segments sorted and inside
//!    the audio.
//! 3. Transcribes the first 30 s with and without `options.timings`: the text
//!    must be the same, the plain final must have no segments, and the timed
//!    one must have one segment per word, in order, inside the clip.
//!
//! Prints a JSON report on stdout and exits 1 if a gate fails.

use std::path::{Path, PathBuf};
use std::time::Instant;

use fotw_speechd_client::message::{CloseReason, FinalText, StreamOptions};
use fotw_speechd_client::{
    DIARIZATION_MAX_UNACKED_MS, SpeechdClient, SpeechdConfig, StreamConfig, StreamEvent,
};
use serde_json::json;

const DIARIZER: &str = "diarizer";
const PARAKEET: &str = "parakeet-unified";
const RATE: usize = 16_000;

struct Args {
    helper: PathBuf,
    models_root: PathBuf,
    voices: Vec<PathBuf>,
    offset_s: usize,
    seconds: usize,
    parakeet_from: Option<PathBuf>,
    diarizer_from: Option<PathBuf>,
}

fn args() -> Args {
    let mut a = Args {
        helper: PathBuf::new(),
        models_root: PathBuf::new(),
        voices: Vec::new(),
        offset_s: 0,
        seconds: 180,
        parakeet_from: None,
        diarizer_from: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .unwrap_or_else(|| panic!("{flag} takes a value"));
        let number = || value.parse().expect("a number");
        match flag.as_str() {
            "--helper" => a.helper = value.into(),
            "--models-root" => a.models_root = value.into(),
            "--voice" => a.voices.push(value.into()),
            "--offset-s" => a.offset_s = number(),
            "--seconds" => a.seconds = number(),
            "--parakeet-from" => a.parakeet_from = Some(value.into()),
            "--diarizer-from" => a.diarizer_from = Some(value.into()),
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(
        a.voices.len() >= 2,
        "--voice at least twice: the smoke needs two speakers"
    );
    a
}

/// 16 kHz mono s16 PCM from a canonical WAV.
fn read_wav(path: &Path) -> Vec<i16> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "{}",
        path.display()
    );
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = at + 8;
        if &bytes[at..at + 4] == b"fmt " {
            let channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            assert_eq!(
                (channels, rate),
                (1, 16_000),
                "{}: resample to 16 kHz mono first",
                path.display()
            );
        } else if &bytes[at..at + 4] == b"data" {
            return bytes[body..(body + size).min(bytes.len())]
                .chunks_exact(2)
                .map(|p| i16::from_le_bytes([p[0], p[1]]))
                .collect();
        }
        at = body + size + size % 2;
    }
    panic!("{}: no data chunk", path.display());
}

/// The voices summed over the same span, halved so it cannot clip.
fn mix(args: &Args) -> Vec<i16> {
    let (start, len) = (args.offset_s * RATE, args.seconds * RATE);
    let mut out = vec![0i32; len];
    for voice in &args.voices {
        let pcm = read_wav(voice);
        assert!(pcm.len() >= start + len, "{} is too short", voice.display());
        for (o, s) in out.iter_mut().zip(&pcm[start..start + len]) {
            *o += i32::from(*s);
        }
    }
    let n = i32::try_from(args.voices.len()).expect("few voices");
    out.into_iter()
        .map(|s| i16::try_from(s / n).expect("an average fits"))
        .collect()
}

/// The single final of a finished stream.
async fn final_of(stream: &mut fotw_speechd_client::SpeechStream) -> FinalText {
    let mut result = None;
    while let Some(event) = stream.next_event().await {
        match event {
            StreamEvent::Final(f) => {
                assert!(result.is_none(), "one final per stream");
                result = Some(f);
            }
            StreamEvent::Closed(CloseReason::Finished) => break,
            StreamEvent::Partial { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    result.expect("a final")
}

async fn transcribe(client: &SpeechdClient, pcm: &[i16], timings: bool) -> FinalText {
    let mut config = StreamConfig::new(PARAKEET, 16_000);
    config.options = StreamOptions::default().with_timings(timings);
    let mut stream = client.open_stream(config).await.expect("open");
    stream.send_audio(pcm).await.expect("audio");
    stream.finish().await.expect("finish");
    final_of(&mut stream).await
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args = args();
    let pcm = mix(&args);
    let audio_ms = (pcm.len() * 1000 / RATE) as u64;
    let mut gates: Vec<(String, bool, String)> = Vec::new();

    let mut config = SpeechdConfig::new(&args.helper, &args.models_root);
    config.client_name = "speechd_postpass_real".into();
    let client = SpeechdClient::start(config).await.expect("start");

    // 1. Setup.
    let caps = client.capabilities().expect("capabilities");
    for (engine, from) in [
        (DIARIZER, args.diarizer_from.as_deref()),
        (PARAKEET, args.parakeet_from.as_deref()),
    ] {
        let info = caps
            .engine(engine)
            .unwrap_or_else(|| panic!("{engine} missing: is the helper built with FluidAudio?"));
        if !info.model.as_ref().is_some_and(|m| m.installed) {
            let revision = client
                .install(engine, from, None)
                .await
                .unwrap_or_else(|e| panic!("install {engine}: {e}"));
            eprintln!("setup: installed {engine}@{revision}");
        }
    }
    let license = caps
        .engine(DIARIZER)
        .and_then(|e| e.model.as_ref())
        .map(|m| m.license.clone());

    // 2. Diarization, paced by the helper's window acknowledgements.
    let t = Instant::now();
    client.load(DIARIZER).await.expect("load diarizer");
    client.warmup(DIARIZER).await.expect("warm diarizer");
    let ready_ms = t.elapsed().as_millis();
    let mut stream = client
        .open_stream(StreamConfig::new(DIARIZER, 16_000))
        .await
        .expect("open diarizer");
    let (mut sent, mut acked_ms, mut acks) = (0u64, 0u64, 0usize);
    for window in pcm.chunks(160_000) {
        stream.send_audio(window).await.expect("audio");
        sent += window.len() as u64;
        let sent_ms = sent * 1000 / RATE as u64;
        if sent_ms > acked_ms + DIARIZATION_MAX_UNACKED_MS {
            acked_ms = stream
                .await_progress(sent_ms - DIARIZATION_MAX_UNACKED_MS)
                .await
                .expect("progress");
            acks += 1;
        }
    }
    stream.finish().await.expect("finish");
    let diarized = final_of(&mut stream).await;
    let mut speakers: Vec<&str> = diarized
        .segments
        .iter()
        .filter_map(|s| s.speaker.as_deref())
        .collect();
    speakers.sort_unstable();
    speakers.dedup();
    let sorted = diarized
        .segments
        .windows(2)
        .all(|w| w[0].start_ms <= w[1].start_ms);
    let inside = diarized
        .segments
        .iter()
        .all(|s| s.start_ms < s.end_ms && s.end_ms <= audio_ms);
    // A label that comes back after another speaker's turn: the stream's
    // speaker tracker carried it across windows rather than starting over.
    let labels: Vec<&str> = diarized
        .segments
        .iter()
        .filter_map(|s| s.speaker.as_deref())
        .collect();
    let reused = labels
        .iter()
        .enumerate()
        .any(|(i, label)| labels[..i].contains(label) && labels[i - 1] != *label);
    gates.push((
        "diarizer finds 2 to 4 speakers in a two-voice mix".into(),
        (2..=4).contains(&speakers.len()),
        format!("{speakers:?}"),
    ));
    gates.push((
        "a speaker's label returns after another's turn".into(),
        reused,
        format!("{labels:?}"),
    ));
    gates.push((
        "diarizer segments sorted and inside the audio".into(),
        sorted && inside && diarized.text.is_empty() && diarized.end_ms == audio_ms,
        format!(
            "{} segments, end_ms {} of {audio_ms}",
            diarized.segments.len(),
            diarized.end_ms
        ),
    ));

    // 3. Word timings.
    client.load(PARAKEET).await.expect("load parakeet");
    client.warmup(PARAKEET).await.expect("warm parakeet");
    let clip = &pcm[..pcm.len().min(30 * RATE)];
    let clip_ms = (clip.len() * 1000 / RATE) as u64;
    let plain = transcribe(&client, clip, false).await;
    let timed = transcribe(&client, clip, true).await;
    // Every timed segment must carry exactly one word, and together they must
    // be the text's words, in order: no segment without text, none holding
    // several words.
    let words: Option<Vec<&str>> = timed
        .segments
        .iter()
        .map(|s| {
            s.text
                .as_deref()
                .filter(|t| !t.is_empty() && !t.contains(char::is_whitespace))
        })
        .collect();
    let text_words: Vec<&str> = timed.text.split_whitespace().collect();
    gates.push((
        "plain final unchanged: no segments".into(),
        plain.segments.is_empty(),
        format!("{} segments", plain.segments.len()),
    ));
    gates.push((
        "timed text equals the plain text, one segment per word".into(),
        !text_words.is_empty() && timed.text == plain.text && words.as_ref() == Some(&text_words),
        format!(
            "{} segments for {} words",
            timed.segments.len(),
            text_words.len()
        ),
    ));
    gates.push((
        "word times ordered and inside the clip".into(),
        timed
            .segments
            .windows(2)
            .all(|w| w[0].start_ms <= w[1].start_ms)
            && timed
                .segments
                .iter()
                .all(|s| s.start_ms <= s.end_ms && s.end_ms <= clip_ms),
        format!(
            "first {:?}, last {:?}",
            timed.segments.first(),
            timed.segments.last()
        ),
    ));
    client.shutdown().await;

    let passed = gates.iter().all(|g| g.1);
    let report = json!({
        "audio_ms": audio_ms,
        "voices": args.voices.iter().map(|v| v.display().to_string()).collect::<Vec<_>>(),
        "offset_s": args.offset_s,
        "diarizer": {
            "model": diarized.model,
            "license": license,
            "ready_ms": ready_ms,
            "paced_waits": acks,
            "speakers": speakers,
            "segments": diarized.segments.len(),
            "first_segments": diarized.segments.iter().take(8).collect::<Vec<_>>(),
        },
        "timings": {
            "text": timed.text,
            "first_words": timed.segments.iter().take(8).collect::<Vec<_>>(),
        },
        "gates": gates.iter().map(|(name, ok, detail)| json!({"gate": name, "pass": ok, "detail": detail})).collect::<Vec<_>>(),
        "passed": passed,
    });
    println!("{}", serde_json::to_string_pretty(&report).expect("json"));
    if !passed {
        std::process::exit(1);
    }
}
