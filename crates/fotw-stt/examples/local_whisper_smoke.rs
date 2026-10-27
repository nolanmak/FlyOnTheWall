//! Explicit local-model smoke check; reads up to 30 minutes of mono 16 kHz f32 PCM.
//! No capture, credentials or provider network. Supply a public fixture.
#[cfg(all(target_os = "macos", feature = "local-whisper"))]
fn main() -> Result<(), String> {
    use fotw_stt::{WhisperEngine, local::RecognizerFactory};
    use std::{
        io::{Read, Write},
        time::Instant,
    };

    let args: Vec<_> = std::env::args().collect();
    let wait_for_start = args.len() == 5 && args[4] == "--wait-for-start";
    if args.len() != 4 && !wait_for_start {
        return Err(
            "Usage: local_whisper_smoke MODEL mono16k.f32le LEGS(1|2) [--wait-for-start]".into(),
        );
    }
    let legs: usize = args[3].parse().map_err(|_| "LEGS must be 1 or 2")?;
    if !(1..=2).contains(&legs) {
        return Err("LEGS must be 1 or 2".into());
    }
    let pcm = read_pcm(std::fs::File::open(&args[2]).map_err(|e| e.to_string())?)?;
    let started = Instant::now();
    let engine = WhisperEngine::new(&args[1], Some("en".into()));
    engine.load().map_err(|e| e.message)?;
    let load_seconds = started.elapsed().as_secs_f64();
    let mut recognizers = Vec::new();
    for _ in 0..legs {
        recognizers.push(engine.create().map_err(|e| e.message)?);
    }
    // An owned QA controller can load both apps' engines before releasing
    // inference. EOF or a wrong byte fails without transcribing the fixture.
    if wait_for_start {
        println!("{}", serde_json::json!({"ready":true,"legs":legs}));
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut gate = [0];
        std::io::stdin()
            .read_exact(&mut gate)
            .map_err(|e| e.to_string())?;
        if gate[0] != b'1' {
            return Err("Expected start byte 1 from the owned QA controller".into());
        }
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

#[cfg(any(test, all(target_os = "macos", feature = "local-whisper")))]
fn read_pcm(input: impl std::io::Read) -> Result<Vec<f32>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    input
        .take(16_000 * 1800 * 4 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.is_empty() || bytes.len() > 16_000 * 1800 * 4 || bytes.len() % 4 != 0 {
        return Err("Expected 0–1800 seconds of complete f32 samples".into());
    }
    let pcm: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    if pcm
        .iter()
        .any(|sample| !sample.is_finite() || sample.abs() > 1.0)
    {
        return Err("PCM samples must be finite and within [-1, 1]".into());
    }
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::read_pcm;
    use std::{
        cell::Cell,
        io::{self, Read},
    };

    #[test]
    fn ten_minute_fixture_keeps_its_tail_and_exact_sample_count() {
        let samples = 16_000 * 600;
        let input = io::repeat(0)
            .take((samples * 4 - 4) as u64)
            .chain(io::Cursor::new(0.25_f32.to_le_bytes()));
        let pcm = read_pcm(input).expect("a ten-minute meeting fixture must be accepted");
        assert_eq!(pcm.len(), samples);
        assert_eq!(pcm[0], 0.0);
        assert_eq!(pcm.last(), Some(&0.25));
    }

    #[test]
    fn over_thirty_minutes_is_refused_with_a_bounded_read() {
        struct Counted<'a> {
            bytes: &'a Cell<usize>,
            remaining: usize,
        }
        impl Read for Counted<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = output.len().min(self.remaining);
                output[..count].fill(0);
                self.remaining -= count;
                self.bytes.set(self.bytes.get() + count);
                Ok(count)
            }
        }
        let count = Cell::new(0);
        let input = Counted {
            bytes: &count,
            remaining: 16_000 * 1801 * 4,
        };
        assert!(read_pcm(input).is_err());
        assert_eq!(
            count.get(),
            16_000 * 1800 * 4 + 1,
            "stop reading at the documented 30-minute bound plus one byte"
        );
    }

    #[test]
    fn malformed_and_invalid_float_samples_are_refused() {
        for bytes in [
            vec![],
            vec![0, 0, 0],
            f32::NAN.to_le_bytes().to_vec(),
            f32::INFINITY.to_le_bytes().to_vec(),
            (-1.01_f32).to_le_bytes().to_vec(),
            1.01_f32.to_le_bytes().to_vec(),
        ] {
            assert!(read_pcm(io::Cursor::new(bytes)).is_err());
        }
        let expected = [-1.0_f32, 0.0, 0.25, 1.0];
        let bytes: Vec<_> = expected
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        assert_eq!(read_pcm(io::Cursor::new(bytes)).unwrap(), expected);
    }
}
