//! Explicit approved-fixture inference; no downloads, device access or providers.
use super::{FRAME, RATE, SAMPLES, WARMUP, delay, room, signal, word_error};
use fotw_stt::{RecognizerFactory, WhisperEngine};
use sha2::{Digest, Sha256};
use std::{ffi::OsString, fs::File, io::Read, path::Path, time::Instant};

const WAV_SHA: &str = "59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e";
const MODEL_SHA: &str = "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d";
const REFERENCE: &str = "And so my fellow Americans ask not what your country can do for you ask what you can do for your country";
const WAV_BYTES: u64 = 352078;
const MODEL_BYTES: u64 = 487614201;

fn checksum(path: &Path, expected: &str, length: u64) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() != length {
        return Err("input does not match the approved public fixture/model length".into());
    }
    let mut input = File::open(path)?.take(length + 1);
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    if format!("{:x}", digest.finalize()) != expected {
        return Err("input does not match the approved public fixture/model checksum".into());
    }
    Ok(())
}

fn rms(samples: &[f32]) -> f64 {
    (samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}

pub(super) fn run(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() != 2 {
        return Err("expected approved JFK WAV and installed small.en model paths".into());
    }
    // Validate both identities before decoding audio or constructing a model.
    checksum(Path::new(&args[0]), WAV_SHA, WAV_BYTES)?;
    checksum(Path::new(&args[1]), MODEL_SHA, MODEL_BYTES)?;
    let wav = fotw_audio::wav::parse(&std::fs::read(&args[0])?)?;
    if wav.format.sample_rate_hz != RATE as u32
        || wav.format.channels != 1
        || wav.samples.len() != RATE * 11
    {
        return Err("approved WAV shape changed".into());
    }
    let end = WARMUP + wav.samples.len();
    assert!(end + FRAME * 2 < SAMPLES);
    let mut near = vec![0.0; SAMPLES];
    near[WARMUP..end].copy_from_slice(&wav.samples);
    let mut far = signal(127);
    let echo = room(&far);
    let far_scale = rms(&wav.samples) / rms(&echo[WARMUP..end]);
    for s in &mut far {
        *s *= far_scale as f32;
    }
    let echo = room(&far);
    let strong = 10f32.powf(6.0 / 20.0);
    let peak = |s: &[f32]| s.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    // One common gain for clean, reference and every mixture. No clipping or
    // per-backend normalization can hide speech attenuation.
    let gain = 0.95 / (peak(&near) + strong * peak(&echo)).max(strong * peak(&far));
    for s in &mut near {
        *s *= gain;
    }
    for s in &mut far {
        *s *= gain;
    }
    let zero = vec![0.0; SAMPLES];
    let engine = WhisperEngine::new(Path::new(&args[1]), Some("en".into()));
    let load = Instant::now();
    engine.load()?;
    let load_ms = load.elapsed().as_secs_f64() * 1000.0;
    let mut rows = Vec::new();
    for backend in [
        "passthrough",
        "gate-100ms",
        "sonora-0.2.0-adapted",
        #[cfg(feature = "native-webrtc")]
        "native-webrtc-2.1.0-aec3",
    ] {
        let control = super::run(backend, &zero, &near);
        let lag = delay(&near, &control);
        let control_error = control[WARMUP + lag..end + lag]
            .iter()
            .zip(&near[WARMUP..end])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            control_error < 1e-5,
            "source speech window was altered or clipped in near-only control"
        );
        let mut cases = vec![
            ("near-only", zero.clone(), near.clone(), None),
            (
                "overlap-0db",
                far.clone(),
                near.iter().zip(room(&far)).map(|(n, e)| n + e).collect(),
                Some(0),
            ),
            (
                "overlap-plus6db",
                far.iter().map(|s| s * strong).collect(),
                near.iter()
                    .zip(room(&far))
                    .map(|(n, e)| n + e * strong)
                    .collect(),
                Some(6),
            ),
        ];
        if backend == "passthrough" {
            cases.push(("muted-control", zero.clone(), zero.clone(), None));
        }
        for (case, reference, mic, echo_db) in cases {
            assert!(peak(&reference) <= 0.951 && peak(&mic) <= 0.951);
            let achieved_db = echo_db.map(|expected| {
                let interference: Vec<_> = mic[WARMUP..end]
                    .iter()
                    .zip(&near[WARMUP..end])
                    .map(|(m, n)| m - n)
                    .collect();
                let db = 20.0 * (rms(&interference) / rms(&near[WARMUP..end])).log10();
                assert!((db - f64::from(expected)).abs() < 0.001);
                db
            });
            let processing = Instant::now();
            let output = super::run(backend, &reference, &mic);
            assert_eq!(output.len(), SAMPLES);
            assert!(output.iter().all(|s| s.is_finite()));
            let processing_ms = processing.elapsed().as_secs_f64() * 1000.0;
            let speech = &output[WARMUP + lag..end + lag];
            assert_eq!(speech.len(), wav.samples.len());
            let mut recognizer = engine.create()?;
            let inference = Instant::now();
            let text = recognizer
                .transcribe(speech)?
                .into_iter()
                .map(|s| s.text)
                .collect::<Vec<_>>()
                .join(" ");
            let inference_ms = inference.elapsed().as_secs_f64() * 1000.0;
            let errors = word_error::compare(REFERENCE, &text);
            if case == "muted-control" && errors.matches * 4 > errors.reference_words {
                return Err(
                    "muted control recognized too much source text for a reliable retention probe"
                        .into(),
                );
            }
            eprintln!("completed public speech case {backend}/{case}");
            rows.push(serde_json::json!({"backend":backend,"case":case,"echo_over_near_db":echo_db,
                "achieved_echo_over_near_db":achieved_db,"mic_peak":peak(&mic),"render_peak":peak(&reference),
                "fixed_delay_samples":lag,"source_speech_samples":speech.len(),
                "near_only_max_sample_error":control_error,"processing_ms":processing_ms,"inference_ms":inference_ms,
                "text":text,"reference_words":errors.reference_words,"matches":errors.matches,
                "substitutions":errors.substitutions,"deletions":errors.deletions,"insertions":errors.insertions,
                "word_error_rate":(errors.substitutions+errors.deletions+errors.insertions) as f64/errors.reference_words as f64,
                "reference_word_recall":errors.matches as f64/errors.reference_words as f64}));
        }
    }
    println!(
        "{}",
        serde_json::json!({"fixture":"approved-public-jfk","wav_sha256":WAV_SHA,
        "hardware":super::hardware(),"recognizer":"fotw-stt WhisperEngine / whisper-rs 0.16.0",
        "model":"small.en","model_sha256":MODEL_SHA,"reference":REFERENCE,
        "reference_source":"public-domain JFK inaugural address excerpt in approved whisper.cpp JFK fixture",
        "far_end":"seeded generated signal 127 through the existing linear room fixture",
        "sample_rate_hz":RATE,"adaptation_seconds":8,"speech_seconds":11,"postroll_seconds":1,
        "common_gain":gain,"model_load_ms":load_ms,"serial_shared_model":true,"app_endpointing_evaluated":false,
        "candidate_selected_for_app":false,"results":rows})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_unapproved_file_is_refused_before_audio_decode_or_model_construction() {
        let path =
            std::env::temp_dir().join(format!("fotw-unapproved-speech-{}", std::process::id()));
        std::fs::write(&path, vec![0; WAV_BYTES as usize]).unwrap();
        let error = run(&[
            path.clone().into_os_string(),
            path.with_extension("missing-model").into_os_string(),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "input does not match the approved public fixture/model checksum"
        );
        std::fs::remove_file(path).unwrap();
    }
}
