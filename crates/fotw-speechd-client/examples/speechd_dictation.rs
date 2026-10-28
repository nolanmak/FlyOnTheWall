//! Murmur #100 real-model check: streaming dictation with `nemotron-3.5`
//! through the helper, replayed in real time. Separate from `ci`; macOS with
//! the FluidAudio helper and the pinned model only.
//!
//! ```text
//! speechd_dictation --helper <fotw-speechd> --models-root <dir> --manifest <corpus-v2/manifest.json>
//!                   [--from <dir with 560ms/ and 1120ms/>] [--music-snr <dB>] [--language <code>]
//! ```
//!
//! 1. **Setup** (the only phase allowed to reach the network): install
//!    `nemotron-3.5`, imported from `--from` when given.
//! 2. Everything after runs the helper under `sandbox-exec` with the network
//!    denied; an install attempt there must fail.
//! 3. **Launch**: spawn, load, warm. Then one clip is dictated six times: the
//!    first dictation after launch against the fifth.
//! 4. **Replay**: every LibriSpeech clip in the manifest (and, with
//!    `--music-snr`, the same clips mixed with music at that SNR) is sent in
//!    10 ms frames at real-time pace, as Murmur sends the microphone. Each
//!    partial's arrival is recorded against the audio clock, then `finish` is
//!    sent and the final timed (release → final).
//!
//! Prints one JSON report on stdout: raw per-clip timings and texts. Onset,
//! percentiles and WER are computed from it by the caller, with the same
//! scoring as `tools/speech-eval`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fotw_speechd_client::message::CloseReason;
use fotw_speechd_client::{SpeechdClient, SpeechdConfig, StreamConfig, StreamEvent};
use serde_json::{Value, json};

const SANDBOX: &str = "(version 1)(allow default)(deny network*)";
const ENGINE: &str = "nemotron-3.5";
const FRAME: usize = 160;

struct Args {
    helper: PathBuf,
    models_root: PathBuf,
    manifest: PathBuf,
    from: Option<PathBuf>,
    music_snr: Option<i64>,
    language: String,
}

fn args() -> Args {
    let mut parsed = Args {
        helper: PathBuf::new(),
        models_root: PathBuf::new(),
        manifest: PathBuf::new(),
        from: None,
        music_snr: None,
        language: "en".into(),
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .unwrap_or_else(|| panic!("{flag} takes a value"));
        let absolute = |v: &str| std::path::absolute(v).unwrap_or_else(|_| v.into());
        match flag.as_str() {
            "--helper" => parsed.helper = absolute(&value),
            "--models-root" => parsed.models_root = absolute(&value),
            "--manifest" => parsed.manifest = absolute(&value),
            "--from" => parsed.from = Some(absolute(&value)),
            "--music-snr" => parsed.music_snr = Some(value.parse().expect("--music-snr dB")),
            "--language" => parsed.language = value,
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(
        !parsed.helper.as_os_str().is_empty() && !parsed.manifest.as_os_str().is_empty(),
        "--helper, --models-root and --manifest are required"
    );
    parsed
}

fn config(args: &Args, sandboxed: bool) -> SpeechdConfig {
    let mut config = SpeechdConfig::new(&args.helper, &args.models_root);
    if sandboxed {
        let mut wrapped = vec![
            "-p".into(),
            SANDBOX.into(),
            args.helper.clone().into_os_string(),
        ];
        wrapped.append(&mut config.args);
        config.program = "/usr/bin/sandbox-exec".into();
        config.args = wrapped;
    }
    config.client_name = "speechd_dictation".into();
    config
}

/// 16 kHz mono s16 PCM from a canonical WAV.
fn read_wav(path: &Path) -> Vec<i16> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if &bytes[at..at + 4] == b"data" {
            return bytes[at + 8..at + 8 + size]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
        }
        at += 8 + size + (size & 1);
    }
    panic!("{}: no data chunk", path.display());
}

/// (path, reference) for the LibriSpeech clips, or their music mixes.
fn clips(manifest: &Path, music_snr: Option<i64>) -> Vec<(PathBuf, String)> {
    let root = manifest.parent().expect("manifest dir");
    let parsed: Value =
        serde_json::from_slice(&std::fs::read(manifest).expect("manifest")).expect("manifest json");
    parsed["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter(|f| match music_snr {
            None => f["category"] == "speech",
            Some(snr) => f["category"] == "speech_in_music" && f["snr_db"] == snr,
        })
        .map(|f| {
            (
                root.join(f["path"].as_str().expect("path")),
                f["reference"].as_str().expect("reference").to_owned(),
            )
        })
        .collect()
}

/// One real-time dictation: (partials as [seconds on the audio clock, text],
/// release → final, final text).
async fn dictate(client: &SpeechdClient, pcm: &[i16], language: &str) -> Value {
    let mut config = StreamConfig::new(ENGINE, 16_000);
    config.options.language = Some(language.to_owned());
    let mut stream = client.open_stream(config).await.expect("open");
    let mut partials = Vec::new();
    let started = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_millis(10));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    for frame in pcm.chunks(FRAME) {
        ticker.tick().await;
        stream.send_audio(frame).await.expect("audio");
        // Take whatever partials have arrived, without waiting.
        while let Ok(Some(event)) = tokio::time::timeout(Duration::ZERO, stream.next_event()).await
        {
            match event {
                StreamEvent::Partial { text, .. } => {
                    partials.push(json!([started.elapsed().as_secs_f64(), text]));
                }
                other => panic!("unexpected {other:?} while streaming"),
            }
        }
    }
    // Release: the last frame is out.
    let released = Instant::now();
    stream.finish().await.expect("finish");
    let mut text = None;
    let mut release_ms = None;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), stream.next_event())
        .await
        .expect("final within 10 s")
    {
        match event {
            StreamEvent::Partial { text, .. } => {
                partials.push(json!([started.elapsed().as_secs_f64(), text]));
            }
            StreamEvent::Final(f) => {
                release_ms = Some(released.elapsed().as_secs_f64() * 1000.0);
                text = Some(f.text);
            }
            StreamEvent::Closed(CloseReason::Finished) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    json!({
        "audio_s": pcm.len() as f64 / 16_000.0,
        "partials": partials,
        "release_ms": release_ms.expect("a final"),
        "text": text.expect("a final"),
    })
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args = args();
    let clips = clips(&args.manifest, args.music_snr);
    assert!(!clips.is_empty(), "no clips in the manifest");

    // 1. Setup, network allowed.
    let setup = SpeechdClient::start(config(&args, false))
        .await
        .expect("start");
    let installed = setup
        .capabilities()
        .and_then(|c| c.engines.into_iter().find(|e| e.id == ENGINE))
        .and_then(|e| e.model)
        .is_some_and(|m| m.installed);
    if !installed {
        setup
            .install(ENGINE, args.from.as_deref(), None)
            .await
            .expect("install nemotron-3.5");
    }
    setup.shutdown().await;

    // 2. Offline from here: the sandbox must refuse an install.
    let launched = Instant::now();
    let client = SpeechdClient::start(config(&args, true))
        .await
        .expect("start sandboxed");
    let sandbox_blocks_install = client.install(ENGINE, None, None).await.is_err();
    let loaded = client.load(ENGINE).await.expect("load");
    let warmed = client.warmup(ENGINE).await.expect("warmup");
    let ready_ms = launched.elapsed().as_secs_f64() * 1000.0;

    // 3. First dictation after launch against the fifth, same clip.
    let probe = read_wav(&clips[0].0);
    let mut launch = Vec::new();
    for _ in 0..6 {
        launch.push(dictate(&client, &probe, &args.language).await);
    }

    // 4. Every clip, in real time.
    let mut replay = Vec::new();
    for (path, reference) in &clips {
        let mut row = dictate(&client, &read_wav(path), &args.language).await;
        row["wav"] = json!(path);
        row["reference"] = json!(reference);
        replay.push(row);
    }
    let health = client.health().await.expect("health");
    client.shutdown().await;

    let report = json!({
        "engine": ENGINE,
        "model": loaded.model,
        "revision": loaded.revision,
        "load_ms": loaded.elapsed.as_secs_f64() * 1000.0,
        "warmup_ms": warmed.as_secs_f64() * 1000.0,
        "spawn_to_ready_ms": ready_ms,
        "sandbox_blocks_install": sandbox_blocks_install,
        "helper_rss_bytes": health.rss_bytes,
        "launch": launch,
        "clips": replay,
    });
    println!("{}", serde_json::to_string_pretty(&report).expect("json"));
}
