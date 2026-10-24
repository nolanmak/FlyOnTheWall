//! Explicit local-model smoke check; reads mono 16 kHz little-endian f32 PCM.
//! No capture, credentials or provider network. Supply a public fixture.
#[cfg(all(target_os = "macos", feature = "local-whisper"))]
fn main() -> Result<(), String> {
    use fotw_stt::{WhisperEngine, local::RecognizerFactory};
    use std::{io::Read, time::Instant};

    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("Usage: local_whisper_smoke MODEL mono16k.f32le LEGS(1|2)".into());
    }
    let legs: usize = args[3].parse().map_err(|_| "LEGS must be 1 or 2")?;
    if !(1..=2).contains(&legs) {
        return Err("LEGS must be 1 or 2".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&args[2])
        .map_err(|e| e.to_string())?
        .take(16_000 * 120 * 4 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.is_empty() || bytes.len() > 16_000 * 120 * 4 || bytes.len() % 4 != 0 {
        return Err("Expected 0–120 seconds of complete f32 samples".into());
    }
    let pcm: Vec<_> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    if pcm
        .iter()
        .any(|sample| !sample.is_finite() || sample.abs() > 1.0)
    {
        return Err("PCM samples must be finite and within [-1, 1]".into());
    }
    let started = Instant::now();
    let engine = WhisperEngine::new(&args[1], Some("en".into()));
    engine.load().map_err(|e| e.message)?;
    let load_seconds = started.elapsed().as_secs_f64();
    let mut recognizers = Vec::new();
    for _ in 0..legs {
        recognizers.push(engine.create().map_err(|e| e.message)?);
    }
    let inference_started = Instant::now();
    let texts = std::thread::scope(|scope| {
        let pcm = &pcm;
        let workers: Vec<_> = recognizers
            .into_iter()
            .map(|mut recognizer| scope.spawn(move || recognizer.transcribe(pcm)))
            .collect();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .map_err(|_| "local recognizer panicked".to_owned())?
                    .map(|segments| {
                        segments
                            .into_iter()
                            .map(|s| s.text)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .map_err(|e| e.message)
            })
            .collect::<Result<Vec<_>, String>>()
    })?;
    println!(
        "{}",
        serde_json::json!({
            "model": engine.model(), "legs": legs, "audio_seconds_per_leg": pcm.len() as f64 / 16_000.0,
            "load_seconds": load_seconds, "inference_seconds": inference_started.elapsed().as_secs_f64(),
            "total_seconds": started.elapsed().as_secs_f64(), "transcripts": texts
        })
    );
    Ok(())
}

#[cfg(not(all(target_os = "macos", feature = "local-whisper")))]
fn main() -> Result<(), String> {
    Err("This smoke check requires macOS and --features local-whisper".into())
}
