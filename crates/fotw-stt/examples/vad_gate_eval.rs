//! Real-model acceptance check for the speech gate (#242). Not part of the
//! ordinary test suite: it needs a Whisper model, the Silero VAD model and a
//! corpus, and it runs for minutes.
//!
//! Runs each clip through the live local pipeline — the same endpointer,
//! worker, speech gate, model and segment cleanup a meeting leg uses — and
//! prints one JSON line per clip with the text it emitted. Scoring is left to
//! the caller (`tools/speech-eval`).
//!
//! ```text
//! cargo run --release -p fotw-stt --features local-whisper --example vad_gate_eval -- \
//!     MODEL (--vad SILERO_MODEL | --no-vad) CLIPS.tsv
//! ```
//!
//! `CLIPS.tsv` holds `id<TAB>/absolute/path.wav` per line; each WAV must be
//! mono 16 kHz PCM16. No capture, credentials or network.
//! `FOTW_EVAL_VAD_THRESHOLD` overrides the gate's start threshold (its
//! continue threshold follows 0.15 below) to explore operating points.

#[cfg(all(target_os = "macos", feature = "local-whisper"))]
fn main() -> Result<(), String> {
    use std::sync::Arc;
    use std::time::Instant;

    use fotw_stt::{LocalStream, LocalStreamConfig, Source, StreamEvent, WhisperEngine};

    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: vad_gate_eval MODEL (--vad SILERO_MODEL | --no-vad) CLIPS.tsv";
    let (model, vad, clips) = match args.as_slice() {
        [_, model, flag, vad, clips] if flag == "--vad" => (model, Some(vad), clips),
        [_, model, flag, clips] if flag == "--no-vad" => (model, None, clips),
        _ => return Err(usage.into()),
    };
    let mut engine = WhisperEngine::new(model, Some("en".into()));
    if let Some(vad) = vad {
        engine = engine.with_speech_gate(vad);
        // Explore the gate's operating point without a rebuild.
        if let Some(threshold) = std::env::var("FOTW_EVAL_VAD_THRESHOLD")
            .ok()
            .and_then(|t| t.parse::<f32>().ok())
        {
            engine = engine.with_gate_config(fotw_stt::GateConfig {
                threshold,
                neg_threshold: threshold - 0.15,
                ..fotw_stt::GateConfig::default()
            });
            eprintln!("gate threshold {threshold}");
        }
    }
    let loading = Instant::now();
    engine.load().map_err(|e| e.message)?;
    eprintln!("loaded in {:.1}s", loading.elapsed().as_secs_f64());
    let engine = Arc::new(engine);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let list = std::fs::read_to_string(clips).map_err(|e| format!("{clips}: {e}"))?;
    for line in list.lines().filter(|l| !l.trim().is_empty()) {
        let (id, path) = line
            .split_once('\t')
            .ok_or_else(|| format!("expected id<TAB>path: {line}"))?;
        let pcm = read_wav(std::path::Path::new(path))?;
        let started = Instant::now();
        let (stream, mut events) = LocalStream::open(
            LocalStreamConfig::new(id, Source::System),
            Arc::clone(&engine) as Arc<dyn fotw_stt::RecognizerFactory>,
        );
        let (texts, errors) = runtime.block_on(async {
            let collect = async {
                let mut texts = Vec::new();
                let mut errors = Vec::new();
                while let Some(event) = events.recv().await {
                    match event {
                        StreamEvent::Final(segment) => texts.push(segment.text),
                        StreamEvent::Error(error) => errors.push(error.to_string()),
                        _ => {}
                    }
                }
                (texts, errors)
            };
            let feed = async {
                // The pump's 20 ms pieces, faster than real time but never
                // more than 2 s ahead of the model, so `close` only waits for
                // the last utterance and stays inside its drain deadline.
                for piece in pcm.chunks(320) {
                    while stream.queued_samples() > 2 * 16_000 {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    stream.write(piece);
                }
                stream.close().await
            };
            let (closed, collected) = tokio::join!(feed, collect);
            let (texts, mut errors) = collected;
            if let Err(error) = closed {
                errors.push(error.to_string());
            }
            (texts, errors)
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
                "errors": errors,
            })
        );
    }
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
    eprintln!("vad_gate_eval needs macOS and the local-whisper feature");
}
