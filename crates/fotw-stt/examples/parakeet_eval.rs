//! Real-model acceptance check for Parakeet Unified as the local meeting
//! engine (#244). Not part of the ordinary test suite: it needs the
//! `fotw-speechd` helper built with FluidAudio, the installed Parakeet and
//! Silero VAD models and the shared corpus, and the replay runs in real time.
//!
//! Both modes run audio through the live local pipeline a meeting leg uses:
//! the endpointer and worker of [`LocalStream`], the Silero speech gate, then
//! one batch stream per utterance on the helper, and the segment cleanup.
//!
//! ```text
//! cargo run --release -p fotw-stt --features local-whisper --example parakeet_eval -- \
//!     clips  --helper BIN --models-root DIR --vad SILERO CLIPS.tsv
//! cargo run --release -p fotw-stt --features local-whisper --example parakeet_eval -- \
//!     replay --helper BIN --models-root DIR --vad SILERO SYSTEM.wav MIC.wav [SECONDS]
//! ```
//!
//! `clips` prints one JSON line per clip (`CLIPS.tsv` holds `id<TAB>/abs.wav`
//! per line), and scoring is left to the caller (`tools/speech-eval`).
//! `replay` feeds two tracks at once, in real time, in the pump's 20 ms pieces,
//! and prints one JSON report: per-utterance lag from the end of its audio to
//! its final, the worst leg backlog, inference time against audio time, and
//! the parent's and helper's process ids so the caller can sample memory.
//! Every WAV must be mono 16 kHz PCM16. No capture, credentials or network.

#[cfg(all(target_os = "macos", feature = "local-whisper"))]
fn main() -> Result<(), String> {
    use std::sync::Arc;
    use std::time::Instant;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage =
        "usage: parakeet_eval (clips|replay) --helper BIN --models-root DIR --vad SILERO ...";
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .ok_or_else(|| format!("{usage}\nmissing {name}"))
    };
    let helper = flag("--helper")?;
    let models_root = flag("--models-root")?;
    let vad = flag("--vad")?;
    let positional: Vec<&String> = {
        let mut out = Vec::new();
        let mut skip = false;
        for (i, a) in args.iter().enumerate() {
            if skip {
                skip = false;
            } else if a.starts_with("--") {
                skip = true;
            } else if i > 0 {
                out.push(a);
            }
        }
        out
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let loading = Instant::now();
    let (engine, timing) = runtime.block_on(async {
        let client = fotw_speechd_client::SpeechdClient::start(
            fotw_speechd_client::SpeechdConfig::new(&helper, &models_root),
        )
        .await
        .map_err(|e| e.to_string())?;
        let engine = fotw_stt::SpeechdEngine::prepare(client, fotw_stt::speechd::PARAKEET_UNIFIED)
            .await
            .map_err(|e| e.message)?;
        let vad = std::path::PathBuf::from(&vad);
        fotw_stt::whisper::SileroDetector::load(&vad).map_err(|e| e.message)?;
        let gated = fotw_stt::vad::GatedFactory::new(Arc::new(engine), move || {
            fotw_stt::whisper::SileroDetector::load(&vad)
                .map(|d| Box::new(d) as Box<dyn fotw_stt::SpeechDetector>)
        });
        let timing = Timing::default();
        Ok::<_, String>((
            Arc::new(Timed {
                inner: Arc::new(gated),
                timing: timing.clone(),
            }) as Arc<dyn fotw_stt::RecognizerFactory>,
            timing,
        ))
    })?;
    eprintln!(
        "helper ready (spawn, load, warmup) in {:.2}s",
        loading.elapsed().as_secs_f64()
    );

    match args.first().map(String::as_str) {
        Some("clips") => {
            let clips = positional.first().ok_or(usage)?;
            clips_mode(&runtime, &engine, clips)
        }
        Some("replay") => {
            let (system, mic) = match positional.as_slice() {
                [s, m, ..] => (s, m),
                _ => return Err(usage.into()),
            };
            let seconds = positional
                .get(2)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(1800.0);
            replay_mode(&runtime, &engine, &timing, system, mic, seconds)
        }
        _ => Err(usage.into()),
    }
}

/// Inference time per leg, measured around the gated recognizer.
#[cfg(all(target_os = "macos", feature = "local-whisper"))]
#[derive(Clone, Default)]
struct Timing(std::sync::Arc<std::sync::Mutex<Vec<(f64, f64)>>>);

#[cfg(all(target_os = "macos", feature = "local-whisper"))]
struct Timed {
    inner: std::sync::Arc<dyn fotw_stt::RecognizerFactory>,
    timing: Timing,
}

#[cfg(all(target_os = "macos", feature = "local-whisper"))]
impl fotw_stt::RecognizerFactory for Timed {
    fn model(&self) -> &str {
        self.inner.model()
    }
    fn language(&self) -> Option<&str> {
        self.inner.language()
    }
    fn provider(&self) -> &str {
        self.inner.provider()
    }
    fn create(&self) -> Result<Box<dyn fotw_stt::Recognizer>, fotw_stt::SttError> {
        Ok(Box::new(TimedRecognizer {
            inner: self.inner.create()?,
            timing: self.timing.clone(),
        }))
    }
}

#[cfg(all(target_os = "macos", feature = "local-whisper"))]
struct TimedRecognizer {
    inner: Box<dyn fotw_stt::Recognizer>,
    timing: Timing,
}

#[cfg(all(target_os = "macos", feature = "local-whisper"))]
impl fotw_stt::Recognizer for TimedRecognizer {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<fotw_stt::Recognized>, fotw_stt::SttError> {
        let started = std::time::Instant::now();
        let result = self.inner.transcribe(pcm);
        self.timing
            .0
            .lock()
            .unwrap()
            .push((pcm.len() as f64 / 16_000.0, started.elapsed().as_secs_f64()));
        result
    }
}

#[cfg(all(target_os = "macos", feature = "local-whisper"))]
fn clips_mode(
    runtime: &tokio::runtime::Runtime,
    engine: &std::sync::Arc<dyn fotw_stt::RecognizerFactory>,
    clips: &str,
) -> Result<(), String> {
    use fotw_stt::{LocalStream, LocalStreamConfig, Source, StreamEvent};
    use std::time::Instant;

    let list = std::fs::read_to_string(clips).map_err(|e| format!("{clips}: {e}"))?;
    for line in list.lines().filter(|l| !l.trim().is_empty()) {
        let (id, path) = line
            .split_once('\t')
            .ok_or_else(|| format!("expected id<TAB>path: {line}"))?;
        let pcm = read_wav(std::path::Path::new(path))?;
        let started = Instant::now();
        let (stream, mut events) = LocalStream::open(
            LocalStreamConfig::new(id, Source::System),
            std::sync::Arc::clone(engine),
        );
        let (texts, provenance, errors) = runtime.block_on(async {
            let collect = async {
                let mut texts = Vec::new();
                let mut provenance = std::collections::BTreeSet::new();
                let mut errors = Vec::new();
                while let Some(event) = events.recv().await {
                    match event {
                        StreamEvent::Final(segment) => {
                            provenance.insert(format!("{}/{}", segment.provider, segment.model));
                            texts.push(segment.text);
                        }
                        StreamEvent::Error(error) => errors.push(error.to_string()),
                        _ => {}
                    }
                }
                (texts, provenance, errors)
            };
            let feed = async {
                for piece in pcm.chunks(320) {
                    while stream.queued_samples() > 2 * 16_000 {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    stream.write(piece);
                }
                stream.close().await
            };
            let (closed, (texts, provenance, mut errors)) = tokio::join!(feed, collect);
            if let Err(error) = closed {
                errors.push(error.to_string());
            }
            (texts, provenance, errors)
        });
        let text = texts.join(" ");
        println!(
            "{}",
            serde_json::json!({
                "id": id,
                "audio_seconds": pcm.len() as f64 / 16_000.0,
                "wall_seconds": started.elapsed().as_secs_f64(),
                "segments": texts.len(),
                "words": text.split_whitespace().count(),
                "text": text,
                "provenance": provenance,
                "errors": errors,
            })
        );
    }
    Ok(())
}

#[cfg(all(target_os = "macos", feature = "local-whisper"))]
fn replay_mode(
    runtime: &tokio::runtime::Runtime,
    engine: &std::sync::Arc<dyn fotw_stt::RecognizerFactory>,
    timing: &Timing,
    system: &str,
    mic: &str,
    seconds: f64,
) -> Result<(), String> {
    use fotw_stt::{LocalStream, LocalStreamConfig, Source, StreamEvent};
    use std::time::{Duration, Instant};

    let limit = (seconds * 16_000.0) as usize;
    let mut tracks = [
        read_wav(std::path::Path::new(system))?,
        read_wav(std::path::Path::new(mic))?,
    ];
    for track in &mut tracks {
        track.truncate(limit);
    }
    let audio_seconds = tracks[0].len().max(tracks[1].len()) as f64 / 16_000.0;
    eprintln!(
        "replaying {audio_seconds:.0}s on two legs in real time; parent pid {}",
        std::process::id()
    );

    let report = runtime.block_on(async {
        let (system_stream, system_events) = LocalStream::open(
            LocalStreamConfig::new("replay", Source::System),
            std::sync::Arc::clone(engine),
        );
        let (mic_stream, mic_events) = LocalStream::open(
            LocalStreamConfig::new("replay", Source::Mic),
            std::sync::Arc::clone(engine),
        );
        let t0 = Instant::now();
        // A final's lag: from when the last sample of its utterance was
        // written (the utterance ends at the segment's end) to its arrival.
        let collect = |mut events: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>| async move {
            let mut lags = Vec::new();
            let mut words = 0usize;
            let mut errors = Vec::new();
            while let Some(event) = events.recv().await {
                match event {
                    StreamEvent::Final(segment) => {
                        let arrived = t0.elapsed().as_secs_f64();
                        let written = segment.end_ms as f64 / 1_000.0;
                        lags.push(arrived - written);
                        words += segment.text.split_whitespace().count();
                    }
                    StreamEvent::Error(error) => errors.push(error.to_string()),
                    _ => {}
                }
            }
            (lags, words, errors)
        };
        let feed = async {
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
            let pieces = tracks[0].len().max(tracks[1].len()).div_ceil(320);
            let mut max_backlog = 0u64;
            for i in 0..pieces {
                ticker.tick().await;
                for (track, stream) in tracks.iter().zip([&system_stream, &mic_stream]) {
                    let from = (i * 320).min(track.len());
                    let to = ((i + 1) * 320).min(track.len());
                    if from < to {
                        stream.write(&track[from..to]);
                    }
                    max_backlog = max_backlog.max(stream.queued_samples());
                }
                if i % 3_000 == 0 {
                    eprintln!("  {:>5.0}s written", i as f64 * 0.02);
                }
            }
            let feeding_done = t0.elapsed().as_secs_f64();
            let closed = (system_stream.close().await, mic_stream.close().await);
            (max_backlog, feeding_done, closed)
        };
        let ((max_backlog, feeding_done, closed), system, mic) =
            tokio::join!(feed, collect(system_events), collect(mic_events));
        let wall = t0.elapsed().as_secs_f64();
        let mut lags: Vec<f64> = system.0.iter().chain(mic.0.iter()).copied().collect();
        lags.sort_by(f64::total_cmp);
        let pct = |p: f64| {
            lags.get(((lags.len() as f64 - 1.0) * p).round() as usize)
                .copied()
                .unwrap_or(f64::NAN)
        };
        let (utterance_audio, inference): (f64, f64) = timing
            .0
            .lock()
            .unwrap()
            .iter()
            .fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
        let mut errors = system.2.clone();
        errors.extend(mic.2.clone());
        for result in [closed.0, closed.1] {
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        serde_json::json!({
            "audio_seconds_per_track": audio_seconds,
            "wall_seconds": wall,
            "drain_after_last_write_seconds": wall - feeding_done,
            "utterances": lags.len(),
            "words": {"system": system.1, "mic": mic.1},
            "lag_seconds": {"p50": pct(0.5), "p95": pct(0.95), "max": lags.last()},
            "max_leg_backlog_seconds": max_backlog as f64 / 16_000.0,
            "utterance_audio_seconds": utterance_audio,
            "inference_seconds": inference,
            "two_track_rtf": inference / audio_seconds,
            "rtf_per_utterance_audio": inference / utterance_audio.max(f64::EPSILON),
            "errors": errors,
        })
    });
    println!("{report}");
    Ok(())
}

/// Mono 16 kHz PCM16 WAV samples.
#[cfg(all(target_os = "macos", feature = "local-whisper"))]
fn read_wav(path: &std::path::Path) -> Result<Vec<i16>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bad = |why: &str| format!("{}: {why}", path.display());
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at =
        |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut at = 12;
    let mut format_ok = false;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32_at(at + 4) as usize;
        let body = at + 8;
        let end = body.checked_add(len).filter(|&e| e <= bytes.len());
        let end = end.ok_or_else(|| bad("truncated chunk"))?;
        if id == b"fmt " {
            format_ok = len >= 16
                && u16_at(body) == 1
                && u16_at(body + 2) == 1
                && u32_at(body + 4) == 16_000
                && u16_at(body + 14) == 16;
        } else if id == b"data" {
            if !format_ok {
                return Err(bad("expected mono 16 kHz PCM16"));
            }
            return Ok(bytes[body..end]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect());
        }
        at = end + (len & 1);
    }
    Err(bad("no data chunk"))
}

#[cfg(not(all(target_os = "macos", feature = "local-whisper")))]
fn main() {
    eprintln!("parakeet_eval needs macOS and the local-whisper feature");
}
