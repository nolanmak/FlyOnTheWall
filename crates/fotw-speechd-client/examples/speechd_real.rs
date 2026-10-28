//! The #243 real-model suite: the FluidAudio helper, pinned models, macOS.
//! Separate from `ci` on purpose; run it with `just speechd-real`.
//!
//! ```text
//! speechd_real --helper <fotw-speechd> --models-root <dir> --corpus <dir>
//!              [--parakeet-from <dir>] [--coreml-cache <dir>] [--trials <n>]
//! ```
//!
//! `--corpus` is a directory with `corpus.json` and `audio/` in the
//! tools/speech-eval layout (16 kHz mono s16 WAV, LibriSpeech test-clean).
//!
//! 1. **Setup** (network allowed, the only phase that may download): install
//!    Parakeet Unified (imported from `--parakeet-from` if given) and Silero VAD.
//! 2. Everything after runs the helper under `sandbox-exec` with the network
//!    denied, while a poller runs `lsof -i` on each helper every 100 ms from
//!    the moment it is spawned, through its loads and warmups:
//!    - **cold start**: spawn to both engines loaded and warmed, after removing
//!      `--coreml-cache` (Core ML's compiled-model cache) if given;
//!    - **warm start**: the same, five times, with the cache in place;
//!    - **first request vs 5th**: one clip transcribed `--trials` times right
//!      after warmup;
//!    - every corpus clip transcribed (a sanity WER), and VAD on speech and on
//!      silence;
//!    - a proof the sandbox works: an install attempt must fail.
//!
//! Prints a JSON report on stdout and exits 1 if a gate fails.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fotw_speechd_client::message::{CloseReason, VadKind};
use fotw_speechd_client::{SpeechdClient, SpeechdConfig, StreamConfig, StreamEvent};
use serde_json::json;

const SANDBOX: &str = "(version 1)(allow default)(deny network*)";
const PARAKEET: &str = "parakeet-unified";
const VAD: &str = "silero-vad";
/// The caption engine, `--caption-engine` (default `parakeet-eou-320ms`).
static CAPTION_ENGINE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

fn caption_engine() -> &'static str {
    CAPTION_ENGINE
        .get()
        .copied()
        .unwrap_or("parakeet-eou-320ms")
}
/// 20 ms at 16 kHz: how the capture pipeline hands audio over.
const FRAME: usize = 320;

struct Args {
    helper: PathBuf,
    models_root: PathBuf,
    corpus: PathBuf,
    parakeet_from: Option<PathBuf>,
    coreml_cache: Option<PathBuf>,
    trials: usize,
    /// With `--captions <corpus dir>`: the #245 live-caption measurements,
    /// on that corpus's LibriSpeech utterances.
    captions: Option<PathBuf>,
    /// Minutes of two-stream real-time captioning for the CPU gate.
    cpu_minutes: u64,
}

fn args() -> Args {
    let mut helper = None;
    let mut models_root = None;
    let mut corpus = None;
    let mut parakeet_from = None;
    let mut coreml_cache = None;
    let mut trials = 25;
    let mut captions = None;
    let mut cpu_minutes = 10;
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = || {
            PathBuf::from(
                iter.next()
                    .unwrap_or_else(|| panic!("{flag} takes a value")),
            )
        };
        match flag.as_str() {
            "--helper" => helper = Some(value()),
            "--models-root" => models_root = Some(value()),
            "--corpus" => corpus = Some(value()),
            "--parakeet-from" => parakeet_from = Some(value()),
            "--coreml-cache" => coreml_cache = Some(value()),
            "--captions" => captions = Some(value()),
            "--caption-engine" => {
                let engine = value().to_string_lossy().into_owned();
                let _ = CAPTION_ENGINE.set(Box::leak(engine.into_boxed_str()));
            }
            "--cpu-minutes" => {
                cpu_minutes = value()
                    .to_string_lossy()
                    .parse()
                    .expect("--cpu-minutes takes a number")
            }
            "--trials" => {
                trials = value()
                    .to_string_lossy()
                    .parse()
                    .expect("--trials takes a number")
            }
            other => panic!("unknown argument {other}"),
        }
    }
    // The helper takes only absolute paths, and a relative one would resolve
    // against wherever it was started.
    let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
    Args {
        helper: absolute(helper.expect("--helper")),
        models_root: absolute(models_root.expect("--models-root")),
        corpus: corpus.expect("--corpus"),
        parakeet_from: parakeet_from.map(absolute),
        coreml_cache,
        trials: trials.max(6),
        captions: captions.map(absolute),
        cpu_minutes,
    }
}

fn config(args: &Args, sandboxed: bool) -> SpeechdConfig {
    let mut config = SpeechdConfig::new(&args.helper, &args.models_root);
    if sandboxed {
        // sandbox-exec execs the helper, so the child pid is the helper's.
        let mut wrapped = vec![
            "-p".into(),
            SANDBOX.into(),
            args.helper.clone().into_os_string(),
        ];
        wrapped.append(&mut config.args);
        config.program = "/usr/bin/sandbox-exec".into();
        config.args = wrapped;
    }
    config.client_name = "speechd_real".into();
    config
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
            assert_eq!((channels, rate), (1, 16_000), "{}", path.display());
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

struct Clip {
    id: String,
    reference: String,
    pcm: Vec<i16>,
}

fn corpus(dir: &Path) -> Vec<Clip> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("corpus.json")).expect("corpus.json"))
            .expect("json");
    manifest["fixtures"]
        .as_array()
        .expect("fixtures")
        .iter()
        .map(|f| Clip {
            id: f["id"].as_str().unwrap().to_owned(),
            reference: f["reference"].as_str().unwrap_or_default().to_owned(),
            pcm: read_wav(&dir.join(f["audio"].as_str().unwrap())),
        })
        .collect()
}

/// Transcribe one clip; returns (text, finish → final latency).
async fn transcribe(client: &SpeechdClient, pcm: &[i16]) -> (String, Duration) {
    let mut stream = client
        .open_stream(StreamConfig::new(PARAKEET, 16_000))
        .await
        .expect("open");
    // 20 ms frames, as the capture pipeline sends them.
    for chunk in pcm.chunks(320) {
        stream.send_audio(chunk).await.expect("audio");
    }
    let finished = Instant::now();
    stream.finish().await.expect("finish");
    let mut text = None;
    let mut latency = Duration::ZERO;
    while let Some(event) = stream.next_event().await {
        match event {
            StreamEvent::Final(f) => {
                latency = finished.elapsed();
                text = Some(f.text);
            }
            StreamEvent::Closed(CloseReason::Finished) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    (text.expect("a final"), latency)
}

/// Speech starts the VAD reports for a clip.
async fn vad_starts(client: &SpeechdClient, pcm: &[i16]) -> usize {
    let mut stream = client
        .open_stream(StreamConfig::new(VAD, 16_000))
        .await
        .expect("open vad");
    for chunk in pcm.chunks(320) {
        stream.send_audio(chunk).await.expect("audio");
    }
    stream.finish().await.expect("finish");
    let mut starts = 0;
    while let Some(event) = stream.next_event().await {
        match event {
            StreamEvent::Vad {
                event: VadKind::SpeechStart,
                ..
            } => starts += 1,
            StreamEvent::Vad { .. } => {}
            StreamEvent::Closed(_) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    starts
}

/// Spawn to both engines loaded and warmed. The socket watch starts as soon
/// as the helper exists, so it covers the loads and warmups too.
async fn ready(
    config: SpeechdConfig,
    seen: &Arc<Mutex<Vec<String>>>,
    engines: &[&str],
) -> (SpeechdClient, Duration, Watch) {
    let started = Instant::now();
    let client = SpeechdClient::start(config).await.expect("start");
    let watch = watch_sockets(client.clone(), seen.clone());
    for &engine in engines {
        client
            .load(engine)
            .await
            .unwrap_or_else(|e| panic!("load {engine}: {e}"));
        client
            .warmup(engine)
            .await
            .unwrap_or_else(|e| panic!("warmup {engine}: {e}"));
    }
    watch.starting.store(false, Ordering::SeqCst);
    (client, started.elapsed(), watch)
}

/// A thread polling `lsof -i` on one client's helper every 100 ms; any line
/// is a network socket. Counts polls while the helper starts (spawn, load,
/// warmup) and after.
struct Watch {
    stop: Arc<AtomicBool>,
    starting: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<(usize, usize)>,
}

impl Watch {
    /// Stop it; (polls while starting, polls after).
    fn finish(self) -> (usize, usize) {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.join().unwrap()
    }
}

fn watch_sockets(client: SpeechdClient, seen: Arc<Mutex<Vec<String>>>) -> Watch {
    let stop = Arc::new(AtomicBool::new(false));
    let starting = Arc::new(AtomicBool::new(true));
    let (thread_stop, thread_starting) = (stop.clone(), starting.clone());
    let handle = std::thread::spawn(move || {
        let (mut while_starting, mut after) = (0, 0);
        while !thread_stop.load(Ordering::SeqCst) {
            if let Some(pid) = client.helper_pid() {
                let out = Command::new("lsof")
                    .args(["-a", "-n", "-P", "-i", "-p", &pid.to_string()])
                    .output();
                if let Ok(out) = out {
                    if thread_starting.load(Ordering::SeqCst) {
                        while_starting += 1;
                    } else {
                        after += 1;
                    }
                    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                    if !text.is_empty() {
                        seen.lock().unwrap().push(text);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        (while_starting, after)
    });
    Watch {
        stop,
        starting,
        handle,
    }
}

fn words(text: &str) -> Vec<String> {
    text.to_uppercase()
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Word edit distance and reference length.
fn word_errors(reference: &str, hypothesis: &str) -> (usize, usize) {
    let (r, h) = (words(reference), words(hypothesis));
    let mut row: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, hw) in h.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(rw != hw)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    (row[h.len()], r.len())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    // Nearest rank, as tools/speech-eval reports.
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank - 1]
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args = args();
    let clips = corpus(&args.corpus);
    let mut gates: Vec<(String, bool, String)> = Vec::new();
    let engines: Vec<&str> = if args.captions.is_some() {
        vec![PARAKEET, VAD, caption_engine()]
    } else {
        vec![PARAKEET, VAD]
    };

    // 1. Setup: the one phase allowed to reach the network.
    let setup = SpeechdClient::start(config(&args, false))
        .await
        .expect("start for setup");
    let caps = setup.capabilities().expect("capabilities");
    for &engine in &engines {
        let info = caps
            .engine(engine)
            .unwrap_or_else(|| panic!("{engine} missing: is the helper built with FluidAudio?"));
        if !info.model.as_ref().is_some_and(|m| m.installed) {
            let from = (engine == PARAKEET)
                .then_some(args.parakeet_from.as_deref())
                .flatten();
            let t = Instant::now();
            let revision = setup
                .install(engine, from, None)
                .await
                .unwrap_or_else(|e| panic!("install {engine}: {e}"));
            eprintln!(
                "setup: installed {engine}@{revision} in {:.1} s",
                t.elapsed().as_secs_f64()
            );
        }
    }
    setup.shutdown().await;

    // 2. Network denied from here on.
    let sockets = Arc::new(Mutex::new(Vec::new()));
    let (mut starting_polls, mut later_polls) = (0, 0);
    let mut tally = |watch: Watch| {
        let (starting, later) = watch.finish();
        starting_polls += starting;
        later_polls += later;
    };

    let cache_cleared = match &args.coreml_cache {
        Some(dir) if dir.exists() => {
            std::fs::remove_dir_all(dir).expect("remove the Core ML cache");
            true
        }
        _ => false,
    };
    let (cold, cold_ms, cold_watch) = ready(config(&args, true), &sockets, &[PARAKEET, VAD]).await;
    gates.push((
        "cold start to ready ≤ 30 s".into(),
        cold_ms <= Duration::from_secs(30),
        format!(
            "{} ms (Core ML cache cleared: {cache_cleared})",
            ms(cold_ms)
        ),
    ));

    // Sandbox proof: an install (a download) must fail in here.
    let denied = cold.install("parakeet-eou-320ms", None, None).await;
    gates.push((
        "network denied in the sandbox".into(),
        denied.is_err(),
        format!("{:?}", denied.err().map(|e| e.to_string())),
    ));
    cold.shutdown().await;
    tally(cold_watch);

    let mut warm_ms = Vec::new();
    let mut warm = None;
    for _ in 0..5 {
        let (client, took, watch) = ready(config(&args, true), &sockets, &[PARAKEET, VAD]).await;
        warm_ms.push(ms(took));
        if let Some((previous, previous_watch)) = warm.replace((client, watch)) {
            SpeechdClient::shutdown(&previous).await;
            tally(previous_watch);
        }
    }
    let (warm, warm_watch) = warm.unwrap();
    let worst_warm = warm_ms.iter().copied().fold(0.0, f64::max);
    gates.push((
        "warm start to ready ≤ 3 s (worst of 5)".into(),
        worst_warm <= 3000.0,
        format!("{warm_ms:?} ms"),
    ));

    // First request after warmup vs the 5th, on one 10 s clip.
    let probe = clips
        .iter()
        .find(|c| c.id.contains("010s"))
        .unwrap_or(&clips[0]);
    let mut latencies = Vec::new();
    for _ in 0..args.trials {
        latencies.push(ms(transcribe(&warm, &probe.pcm).await.1));
    }
    let (first, fifth) = (latencies[0], latencies[4]);
    gates.push((
        "first request ≤ 5th + 20%".into(),
        first <= fifth * 1.2,
        format!("first {first} ms, 5th {fifth} ms ({})", probe.id),
    ));
    let mut steady: Vec<f64> = latencies[1..].to_vec();
    steady.sort_by(f64::total_cmp);

    // Every clip, for a sanity WER under the sandbox.
    let (mut errors, mut total) = (0, 0);
    let mut per_clip = Vec::new();
    for clip in &clips {
        let (text, latency) = transcribe(&warm, &clip.pcm).await;
        let (e, n) = word_errors(&clip.reference, &text);
        errors += e;
        total += n;
        per_clip.push(json!({"id": clip.id, "audio_s": clip.pcm.len() as f64 / 16_000.0, "latency_ms": ms(latency), "word_errors": e, "words": n}));
    }
    let wer = errors as f64 / total.max(1) as f64;
    gates.push((
        "transcribes offline (sanity WER ≤ 10%)".into(),
        wer <= 0.10,
        format!("{:.2}% over {} clips", wer * 100.0, clips.len()),
    ));

    let speech_starts = vad_starts(&warm, &probe.pcm).await;
    let silence_starts = vad_starts(&warm, &vec![0i16; 16_000 * 10]).await;
    gates.push((
        "VAD: speech found, silence quiet".into(),
        speech_starts >= 1 && silence_starts == 0,
        format!("speech {speech_starts} start(s), 10 s silence {silence_starts}"),
    ));

    let mut caption_report = serde_json::Value::Null;
    if let Some(dir) = &args.captions {
        // Loaded only now, so the #243 start-up gates above measure exactly
        // what they always have.
        let engine = caption_engine();
        let loaded = Instant::now();
        warm.load(engine)
            .await
            .unwrap_or_else(|e| panic!("load {engine}: {e}"));
        warm.warmup(engine)
            .await
            .unwrap_or_else(|e| panic!("warmup {engine}: {e}"));
        eprintln!(
            "captions: {engine} loaded and warmed in {} ms",
            ms(loaded.elapsed())
        );
        caption_report = captions(&warm, dir, args.cpu_minutes, &mut gates).await;
    }

    let health = warm.health().await.expect("health");
    warm.shutdown().await;
    tally(warm_watch);
    let sockets = sockets.lock().unwrap().clone();
    gates.push((
        "no network socket after setup (lsof, from spawn on)".into(),
        sockets.is_empty() && starting_polls > 0 && later_polls > 0,
        format!(
            "{starting_polls} polls during spawn/load/warmup of 6 helpers, {later_polls} after; {} with sockets",
            sockets.len()
        ),
    ));

    let pass = gates.iter().all(|g| g.1);
    let report = json!({
        "gates": gates.iter().map(|(name, ok, detail)| json!({"gate": name, "pass": ok, "measured": detail})).collect::<Vec<_>>(),
        "cold_start_ms": ms(cold_ms),
        "warm_start_ms": warm_ms,
        "requests": {"clip": probe.id, "trials": latencies.len(), "first_ms": first, "fifth_ms": fifth,
                      "warm_p50_ms": percentile(&steady, 50.0), "warm_p95_ms": percentile(&steady, 95.0), "all_ms": latencies},
        "corpus": per_clip,
        "helper_rss_bytes": health.rss_bytes,
        "lsof": sockets,
        "captions": caption_report,
        "pass": pass,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    for (name, ok, detail) in &gates {
        eprintln!("{} {name}: {detail}", if *ok { "PASS" } else { "FAIL" });
    }
    std::process::exit(i32::from(!pass));
}

// ------------------------------------------------------------------ #245

/// Which 20 ms frames of `pcm` are voiced: RMS within 30 dB of the loudest
/// frame. A recording's own leading silence and mid-sentence pauses are not.
fn voiced_frames(pcm: &[i16]) -> Vec<bool> {
    let rms: Vec<f64> = pcm
        .chunks(FRAME)
        .map(|f| (f.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / f.len() as f64).sqrt())
        .collect();
    let loudest = rms.iter().copied().fold(0.0, f64::max);
    rms.iter().map(|&r| r > loudest / 31.6).collect()
}

/// Caption timing for one utterance.
struct CaptionTiming {
    /// First voiced frame sent → first partial.
    first: Option<f64>,
    /// End of the inserted silence → first partial (counts the recording's
    /// own leading silence too).
    first_from_audio: Option<f64>,
    /// Gaps between partials during continuous speech: no pause over 300 ms
    /// between them.
    gaps: Vec<f64>,
    /// Every gap while speech was arriving, pauses included.
    all_gaps: Vec<f64>,
}

/// One utterance captioned in real time: 1 s of silence, the speech, 1 s of
/// silence, in 20 ms frames.
async fn caption_utterance(client: &SpeechdClient, speech: &[i16]) -> CaptionTiming {
    let lead = 16_000;
    let mut audio = vec![0i16; lead];
    audio.extend_from_slice(speech);
    audio.extend(std::iter::repeat_n(0i16, 16_000));
    let speech_end = lead + speech.len();
    let voiced = voiced_frames(speech);
    let first_voiced = lead + voiced.iter().position(|&v| v).unwrap_or(0) * FRAME;
    let mut stream = client
        .open_stream(StreamConfig::new(caption_engine(), 16_000))
        .await
        .expect("open a caption stream");
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let (mut sent, mut onset, mut voice_onset, mut ended) = (0usize, None, None, None);
    // Each partial's arrival, with how much audio had been sent by then.
    let mut partials: Vec<(Instant, usize)> = Vec::new();
    loop {
        tokio::select! {
            _ = tick.tick(), if sent < audio.len() => {
                let frame = &audio[sent..(sent + FRAME).min(audio.len())];
                stream.send_audio(frame).await.expect("caption audio");
                sent += frame.len();
                if onset.is_none() && sent > lead {
                    onset = Some(Instant::now());
                }
                if voice_onset.is_none() && sent > first_voiced {
                    voice_onset = Some(Instant::now());
                }
                if ended.is_none() && sent >= speech_end {
                    ended = Some(Instant::now());
                }
                if sent == audio.len() {
                    stream.finish().await.expect("finish captions");
                }
            }
            event = stream.next_event() => match event {
                Some(StreamEvent::Partial { .. }) => {
                    if onset.is_some() {
                        partials.push((Instant::now(), sent));
                    }
                }
                Some(StreamEvent::Final(_) | StreamEvent::Vad { .. }) => {}
                Some(StreamEvent::Closed(_)) | None => break,
                Some(StreamEvent::Failed(f)) => panic!("caption stream failed: {}", f.message),
            }
        }
    }
    let onset = onset.expect("speech was sent");
    let voice_onset = voice_onset.expect("speech was sent");
    let first = partials
        .iter()
        .find(|(at, _)| *at >= voice_onset)
        .map(|(at, _)| ms(at.duration_since(voice_onset)));
    let first_from_audio = partials.first().map(|(at, _)| ms(at.duration_since(onset)));
    let speaking: Vec<(Instant, usize)> = partials
        .iter()
        .copied()
        .filter(|(at, _)| ended.is_none_or(|end| *at <= end))
        .collect();
    let all_gaps = speaking
        .windows(2)
        .map(|w| ms(w[1].0.duration_since(w[0].0)))
        .collect();
    // Longest run of unvoiced frames in the speech between two send positions.
    let longest_pause = |from: usize, to: usize| {
        let frames = |p: usize| p.saturating_sub(lead) / FRAME;
        let (mut run, mut longest) = (0, 0);
        for &v in voiced.iter().take(frames(to)).skip(frames(from)) {
            run = if v { 0 } else { run + 1 };
            longest = longest.max(run);
        }
        longest
    };
    let gaps = speaking
        .windows(2)
        .filter(|w| longest_pause(w[0].1, w[1].1) * 20 <= 300)
        .map(|w| ms(w[1].0.duration_since(w[0].0)))
        .collect();
    CaptionTiming {
        first,
        first_from_audio,
        gaps,
        all_gaps,
    }
}

/// CPU seconds the process has used, from `ps`.
fn cpu_seconds(pid: u32) -> f64 {
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    // [[H:]M]M:SS.ss
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .split(':')
        .fold(0.0, |total, part| {
            total * 60.0 + part.parse::<f64>().unwrap_or(0.0)
        })
}

/// Two caption streams (mic and system) in real time for `minutes`; the
/// helper's CPU use over that time as a share of one core.
async fn caption_cpu(client: &SpeechdClient, audio: &[i16], minutes: u64) -> f64 {
    let pid = client.helper_pid().expect("a helper");
    let mut streams = Vec::new();
    for _ in 0..2 {
        streams.push(
            client
                .open_stream(StreamConfig::new(caption_engine(), 16_000))
                .await
                .expect("open a caption stream"),
        );
    }
    let (cpu0, t0) = (cpu_seconds(pid), Instant::now());
    let frames = (minutes * 60 * 1000 / 20) as usize;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut at = 0usize;
    for _ in 0..frames {
        tick.tick().await;
        for (i, stream) in streams.iter_mut().enumerate() {
            // The two legs play different parts of the same speech.
            let offset = (at + i * audio.len() / 2) % (audio.len() - FRAME);
            stream
                .send_audio(&audio[offset..offset + FRAME])
                .await
                .expect("caption audio");
            while let Ok(Some(event)) =
                tokio::time::timeout(Duration::ZERO, stream.next_event()).await
            {
                if let StreamEvent::Failed(f) = event {
                    panic!("caption stream failed: {}", f.message);
                }
            }
        }
        at += FRAME;
    }
    let share = (cpu_seconds(pid) - cpu0) / t0.elapsed().as_secs_f64() * 100.0;
    for mut stream in streams {
        stream.cancel();
    }
    share
}

/// The #245 gates: caption latency, final text latency and caption CPU.
async fn captions(
    client: &SpeechdClient,
    dir: &Path,
    cpu_minutes: u64,
    gates: &mut Vec<(String, bool, String)>,
) -> serde_json::Value {
    // LibriSpeech utterances only, as the epoch's Delivery rules ask.
    let utterances: Vec<Clip> = corpus(dir)
        .into_iter()
        .filter(|c| c.id.starts_with("ls-") && c.pcm.len() <= 20 * 16_000)
        .collect();
    assert!(
        utterances.len() >= 20,
        "need at least 20 LibriSpeech utterances, found {}",
        utterances.len()
    );

    let (mut firsts, mut gaps, mut finals, mut missing) = (Vec::new(), Vec::new(), Vec::new(), 0);
    let (mut firsts_from_audio, mut all_gaps) = (Vec::new(), Vec::new());
    for clip in &utterances {
        let timing = caption_utterance(client, &clip.pcm).await;
        match timing.first {
            Some(first) => firsts.push(first),
            None => missing += 1,
        }
        firsts_from_audio.extend(timing.first_from_audio);
        gaps.extend(timing.gaps);
        all_gaps.extend(timing.all_gaps);
        // The end of an utterance as VAD reports it is where the final engine
        // gets the utterance: finish → final is the wait for its text.
        finals.push(ms(transcribe(client, &clip.pcm).await.1));
    }
    for list in [
        &mut firsts,
        &mut gaps,
        &mut finals,
        &mut firsts_from_audio,
        &mut all_gaps,
    ] {
        list.sort_by(f64::total_cmp);
    }
    let p = |list: &[f64], q| {
        if list.is_empty() {
            f64::INFINITY
        } else {
            percentile(list, q)
        }
    };
    let (first_p95, gap_p95, final_p95) = (p(&firsts, 95.0), p(&gaps, 95.0), p(&finals, 95.0));
    gates.push((
        "captions: speech onset → first partial p95 ≤ 800 ms".into(),
        first_p95 <= 800.0 && missing == 0,
        format!(
            "p50 {} / p95 {first_p95} ms over {} utterances ({missing} without a partial)",
            p(&firsts, 50.0),
            utterances.len()
        ),
    ));
    gates.push((
        "captions: between partials in continuous speech p95 ≤ 500 ms".into(),
        gap_p95 <= 500.0,
        format!(
            "p50 {} / p95 {gap_p95} ms over {} gaps",
            p(&gaps, 50.0),
            gaps.len()
        ),
    ));
    gates.push((
        "utterance end → final text p95 ≤ 1.5 s".into(),
        final_p95 <= 1500.0,
        format!(
            "p50 {} / p95 {final_p95} ms (parakeet-unified)",
            p(&finals, 50.0)
        ),
    ));

    let audio: Vec<i16> = utterances
        .iter()
        .flat_map(|c| c.pcm.iter().copied())
        .collect();
    let cpu = caption_cpu(client, &audio, cpu_minutes).await;
    gates.push((
        format!("captions: CPU ≤ 25% of one core ({cpu_minutes} min, two streams)"),
        cpu <= 25.0,
        format!("{cpu:.1}% of one core"),
    ));
    json!({
        "engine": caption_engine(),
        "utterances": utterances.len(),
        "first_partial_ms": {"p50": p(&firsts, 50.0), "p95": first_p95, "missing": missing,
                             "from_first_voiced_frame": true},
        "first_partial_from_audio_start_ms": {"p50": p(&firsts_from_audio, 50.0), "p95": p(&firsts_from_audio, 95.0)},
        "between_partials_ms": {"p50": p(&gaps, 50.0), "p95": gap_p95, "count": gaps.len(),
                                "excludes_pauses_over_ms": 300},
        "between_partials_all_ms": {"p50": p(&all_gaps, 50.0), "p95": p(&all_gaps, 95.0), "count": all_gaps.len()},
        "final_ms": {"p50": p(&finals, 50.0), "p95": final_p95},
        "cpu_percent_of_one_core": cpu,
        "cpu_minutes": cpu_minutes,
    })
}
