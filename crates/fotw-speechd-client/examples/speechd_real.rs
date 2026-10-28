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

struct Args {
    helper: PathBuf,
    models_root: PathBuf,
    corpus: PathBuf,
    parakeet_from: Option<PathBuf>,
    coreml_cache: Option<PathBuf>,
    trials: usize,
}

fn args() -> Args {
    let mut helper = None;
    let mut models_root = None;
    let mut corpus = None;
    let mut parakeet_from = None;
    let mut coreml_cache = None;
    let mut trials = 25;
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
) -> (SpeechdClient, Duration, Watch) {
    let started = Instant::now();
    let client = SpeechdClient::start(config).await.expect("start");
    let watch = watch_sockets(client.clone(), seen.clone());
    for engine in [PARAKEET, VAD] {
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

    // 1. Setup: the one phase allowed to reach the network.
    let setup = SpeechdClient::start(config(&args, false))
        .await
        .expect("start for setup");
    let caps = setup.capabilities().expect("capabilities");
    for engine in [PARAKEET, VAD] {
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
    let (cold, cold_ms, cold_watch) = ready(config(&args, true), &sockets).await;
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
        let (client, took, watch) = ready(config(&args, true), &sockets).await;
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
        "pass": pass,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    for (name, ok, detail) in &gates {
        eprintln!("{} {name}: {detail}", if *ok { "PASS" } else { "FAIL" });
    }
    std::process::exit(i32::from(!pass));
}
