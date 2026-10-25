//! Deterministic signal-level AEC comparison. No capture, network, models or user audio.
use fotw_pipeline::aec::{Adapter, Backend, Config as AdapterConfig, OutputKind};
use fotw_pipeline::echo::{EchoGate, GateVerdict};
use sonora::{AudioProcessing, Config, StreamConfig, config::EchoCanceller};
use std::time::Instant;

const RATE: usize = 16000;
const FRAME: usize = RATE / 100;
const SAMPLES: usize = RATE * 20;
const WARMUP: usize = RATE * 8;

fn sonora_processor() -> AudioProcessing {
    AudioProcessing::builder()
        .config(Config {
            echo_canceller: Some(EchoCanceller {
                enforce_high_pass_filtering: false,
                ..Default::default()
            }),
            ..Default::default()
        })
        .capture_config(StreamConfig::new(RATE as u32, 1))
        .render_config(StreamConfig::new(RATE as u32, 1))
        .build()
}

struct StreamingSonora {
    processor: AudioProcessing,
    render_output: [f32; FRAME],
}
impl Backend for StreamingSonora {
    type Error = sonora::Error;
    fn frame_samples(&self) -> usize {
        StreamConfig::new(RATE as u32, 1).num_frames()
    }
    // Control-validated delay for this pinned 16 kHz configuration. A real
    // backend test below rejects a changed delay or an undrained source tail.
    fn delay_samples(&self) -> usize {
        128
    }
    fn analyze_render(&mut self, frame: &[f32]) -> Result<(), Self::Error> {
        self.processor
            .process_render_f32(&[frame], &mut [&mut self.render_output])
    }
    fn process_capture(&mut self, frame: &[f32], output: &mut [f32]) -> Result<(), Self::Error> {
        self.processor.process_capture_f32(&[frame], &mut [output])
    }
    fn reset(&mut self) -> Result<(), Self::Error> {
        self.processor = sonora_processor();
        Ok(())
    }
}

fn random(state: &mut u32) -> f32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state as f32 / u32::MAX as f32
}

fn signal(seed: u32) -> Vec<f32> {
    let mut noise = seed;
    let mut envelope = seed.wrapping_mul(747796405).wrapping_add(1);
    let (mut fast, mut slow, mut level, mut target) = (0.0, 0.0, 0.4, 0.4);
    (0..SAMPLES)
        .map(|i| {
            if i % 2400 == 0 {
                target = random(&mut envelope).powi(2);
            }
            level += 0.002 * (target - level);
            fast += 0.35 * (random(&mut noise) * 2.0 - 1.0 - fast);
            slow += 0.08 * (fast - slow);
            0.8 * (fast - slow) * (0.1 + 0.9 * level)
        })
        .collect()
}

fn room(far: &[f32]) -> Vec<f32> {
    (0..far.len())
        .map(|i| {
            [(640, 0.6), (1120, 0.25), (1560, 0.1)]
                .iter()
                .map(|(delay, gain)| i.checked_sub(*delay).map_or(0.0, |j| far[j] * gain))
                .sum()
        })
        .collect()
}

fn run(backend: &str, far: &[f32], mic: &[f32]) -> Vec<f32> {
    match backend {
        "passthrough" => mic.to_vec(),
        "gate-100ms" => {
            let mut gate = EchoGate::new(RATE as u32);
            let mut out = Vec::with_capacity(mic.len());
            for (render, capture) in far.chunks(1600).zip(mic.chunks(1600)) {
                gate.push_system(render);
                if gate.assess(capture) == GateVerdict::Suppress {
                    out.resize(out.len() + capture.len(), 0.0);
                } else {
                    out.extend_from_slice(capture);
                }
            }
            out
        }
        "sonora-0.2.0-aec3" => {
            let mut aec = sonora_processor();
            let mut render_out = [0.0; FRAME];
            let mut capture_out = [0.0; FRAME];
            let mut out = Vec::with_capacity(mic.len());
            for (render, capture) in far.chunks_exact(FRAME).zip(mic.chunks_exact(FRAME)) {
                aec.process_render_f32(&[render], &mut [&mut render_out])
                    .unwrap();
                aec.process_capture_f32(&[capture], &mut [&mut capture_out])
                    .unwrap();
                out.extend_from_slice(&capture_out);
            }
            out
        }
        "sonora-0.2.0-adapted" => run_streaming(far, mic),
        #[cfg(feature = "native-webrtc")]
        "native-webrtc-2.1.0-aec3" => run_native(far, mic),
        _ => panic!("unknown evaluation backend"),
    }
}

#[cfg(feature = "native-webrtc")]
fn run_native(far: &[f32], mic: &[f32]) -> Vec<f32> {
    use webrtc_audio_processing::{Config, Processor, config::EchoCanceller};
    assert_eq!(far.len(), mic.len());
    assert!(
        mic.len().is_multiple_of(FRAME),
        "fixed-frame control cannot drop a tail"
    );
    let processor = Processor::new(RATE as u32).unwrap();
    assert_eq!(processor.num_samples_per_frame(), FRAME);
    processor.set_config(Config {
        echo_canceller: Some(EchoCanceller::Full {
            stream_delay_ms: None,
        }),
        ..Default::default()
    });
    let mut output = Vec::with_capacity(mic.len());
    for (render, capture) in far.chunks_exact(FRAME).zip(mic.chunks_exact(FRAME)) {
        // Analyze the immutable reference before its matching capture frame.
        processor.analyze_render_frame([render]).unwrap();
        let mut cleaned = capture.to_vec();
        processor
            .process_capture_frame([cleaned.as_mut_slice()])
            .unwrap();
        output.extend(cleaned);
    }
    output
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.max(1e-12).log10()
}

fn run_streaming(far: &[f32], mic: &[f32]) -> Vec<f32> {
    assert_eq!(far.len(), mic.len());
    let backend = StreamingSonora {
        processor: sonora_processor(),
        render_output: [0.0; FRAME],
    };
    let config = AdapterConfig {
        frame_samples: FRAME,
        delay_samples: 128,
        max_wait_samples: 320,
        max_reference_samples: 640,
    };
    let mut adapter = Adapter::new(backend, config, 1, 0).unwrap();
    let mut output = Vec::new();
    let mut at = 0;
    let mut chunk = 0;
    while at < mic.len() {
        let n = [37, 243, 79, 211][chunk % 4].min(mic.len() - at);
        adapter
            .push_render(1, at as u64, &far[at..at + n], &mut output)
            .unwrap();
        adapter
            .push_capture(1, at as u64, &mic[at..at + n], &mut output)
            .unwrap();
        assert!(adapter.buffered_samples() <= 128 + 320 + 160 + 640);
        at += n;
        chunk += 1;
    }
    adapter.finish(&mut output);
    assert_eq!(
        adapter.stats().raw_samples,
        0,
        "aligned synthetic reference must not bypass processing"
    );
    let mut end = 0;
    for block in &output {
        assert_eq!(block.start_sample, end);
        assert_eq!(block.epoch, 1);
        assert_eq!(block.kind, OutputKind::Cleaned);
        end += block.samples.len() as u64;
    }
    assert_eq!(end, mic.len() as u64);
    output.into_iter().flat_map(|b| b.samples).collect()
}

/// Estimate a backend's fixed delay ONLY from its independent near-only control.
/// Reuse that delay for echo/double-talk so each case cannot choose a better fit.
fn delay(near: &[f32], output: &[f32]) -> usize {
    (0..=FRAME * 2)
        .max_by(|a, b| {
            let correlation = |lag: usize| {
                (WARMUP..near.len() - FRAME * 2)
                    .map(|i| f64::from(near[i]) * f64::from(output[i + lag]))
                    .sum::<f64>()
            };
            correlation(*a).total_cmp(&correlation(*b))
        })
        .unwrap()
}

fn metrics(near: &[f32], echo: &[f32], output: &[f32], lag: usize) -> serde_json::Value {
    assert_eq!(
        near.len(),
        output.len(),
        "processing changed the output buffer length"
    );
    assert!(output.iter().all(|v| v.is_finite()));
    let range = WARMUP..near.len() - FRAME * 2;
    let near_power = range
        .clone()
        .map(|i| f64::from(near[i]).powi(2))
        .sum::<f64>();
    let echo_power = range
        .clone()
        .map(|i| f64::from(echo[i]).powi(2))
        .sum::<f64>();
    let gain = if near_power > 1e-12 {
        range
            .clone()
            .map(|i| f64::from(near[i]) * f64::from(output[i + lag]))
            .sum::<f64>()
            / near_power
    } else {
        0.0
    };
    let floor = range.len() as f64 * 1e-12; // -120 dBFS per-sample residual floor.
    // During double-talk this includes near-end distortion as well as echo;
    // it must not be presented as an isolated acoustic echo measurement.
    let residual = range
        .map(|i| (f64::from(output[i + lag]) - gain * f64::from(near[i])).powi(2))
        .sum::<f64>()
        .max(floor);
    serde_json::json!({
        "near_gain": (near_power > 1e-12).then_some(gain),
        "near_gain_db": (near_power > 1e-12).then(|| db(gain * gain)),
        "residual_reduction_db": (echo_power > 1e-12).then(|| db(echo_power / residual)),
        "near_snr_db": (near_power > 1e-12).then(|| db(gain * gain * near_power / residual)),
        "output_buffer_samples": output.len(), "fixed_delay_samples": lag,
    })
}

fn main() {
    let far = signal(127);
    let near = signal(915);
    let echo = room(&far);
    let zero = vec![0.0; SAMPLES];
    let hardware = if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    } else {
        None
    };
    let mut results = Vec::new();
    let backends = [
        "passthrough",
        "gate-100ms",
        "sonora-0.2.0-aec3",
        "sonora-0.2.0-adapted",
        #[cfg(feature = "native-webrtc")]
        "native-webrtc-2.1.0-aec3",
    ];
    for backend in backends {
        let adapted = backend == "sonora-0.2.0-adapted";
        let began = Instant::now();
        let control = run(backend, &zero, &near);
        let wall_ms = began.elapsed().as_secs_f64() * 1000.0;
        let lag = delay(&near, &control);
        results.push(serde_json::json!({"backend":backend, "case":"near-only",
            "metrics":metrics(&near, &zero, &control, lag), "wall_ms":wall_ms,
            "callback_adapter":adapted,"backend_tail_drained":adapted}));
        for (case, clean_near) in [("echo-only", &zero), ("double-talk", &near)] {
            let mic: Vec<_> = clean_near.iter().zip(&echo).map(|(n, e)| n + e).collect();
            let began = Instant::now();
            let out = run(backend, &far, &mic);
            let wall_ms = began.elapsed().as_secs_f64() * 1000.0;
            results.push(serde_json::json!({"backend":backend,"case":case,
                "metrics":metrics(clean_near, &echo, &out, lag), "wall_ms":wall_ms,
                "callback_adapter":adapted,"backend_tail_drained":adapted}));
        }
    }
    println!(
        "{}",
        serde_json::json!({"synthetic_only":true, "hardware":hardware,
        "sample_rate_hz":RATE, "audio_seconds_per_case":20,"warmup_seconds":8,
        "ns_agc_disabled":true,"candidate_selected_for_app":false,
        "native_webrtc_evaluated":cfg!(feature = "native-webrtc"),
        "undrained_controls_present":true,"callback_adapter_evaluated":true,"results":results})
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "native-webrtc")]
    #[test]
    fn native_aec_reduces_echo_without_mutating_its_render_reference() {
        let far = signal(127);
        let original = far.clone();
        let echo = room(&far);
        let zero = vec![0.0; SAMPLES];
        let near = signal(915);
        let control = run_native(&zero, &near);
        let lag = delay(&near, &control);
        let retained = metrics(&near, &zero, &control, lag);
        assert!(
            retained["near_gain_db"].as_f64().unwrap().abs() <= 0.5,
            "echo cancellation cannot pass by muting local speech: {retained}"
        );
        let out = run_native(&far, &echo);
        assert_eq!(far, original);
        assert_eq!(out.len(), echo.len());
        let measured = metrics(&zero, &echo, &out, lag);
        assert!(
            measured["residual_reduction_db"].as_f64().unwrap() >= 15.0,
            "native AEC must cancel the known echo fixture: {measured}"
        );
    }
    #[test]
    fn the_real_backend_adapter_preserves_the_last_near_only_samples_at_partial_stop() {
        let mut near = signal(42);
        near.truncate(RATE + FRAME - 1);
        let out = run_streaming(&vec![0.0; near.len()], &near);
        assert_eq!(out.len(), near.len());
        let tail_error = out[out.len() - FRAME..]
            .iter()
            .zip(&near[near.len() - FRAME..])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            tail_error < 1e-5,
            "delayed or clipped real-backend source tail: max error {tail_error}"
        );
    }
    #[test]
    fn near_snr_uses_the_retained_signal_power_when_the_backend_attenuates_speech() {
        let near: Vec<_> = (0..SAMPLES)
            .map(|i| if i % 2 == 0 { 0.2 } else { -0.2 })
            .collect();
        let output: Vec<_> = near
            .iter()
            .enumerate()
            .map(|(i, n)| 0.5 * n + if i % 4 < 2 { 0.1 } else { -0.1 })
            .collect();
        let result = metrics(&near, &vec![0.0; SAMPLES], &output, 0);
        assert!((result["near_gain"].as_f64().unwrap() - 0.5).abs() < 1e-6);
        assert!(
            result["near_snr_db"].as_f64().unwrap().abs() < 1e-6,
            "equal retained near and residual powers must measure 0 dB SNR: {result}"
        );
    }
    #[test]
    fn suppression_is_not_mistaken_for_near_end_preservation() {
        let near = signal(42);
        let zero = vec![0.0; near.len()];
        let kept = metrics(&near, &zero, &near, 0);
        let deleted = metrics(&near, &zero, &zero, 0);
        assert_eq!(kept["near_gain"], 1.0);
        assert_eq!(deleted["near_gain"], 0.0);
        assert!(deleted["near_gain_db"].as_f64().unwrap() < -100.0);
        assert!(deleted["near_snr_db"].as_f64().unwrap() < -100.0);
    }
    #[test]
    fn unchanged_echo_is_zero_db_improvement_and_clean_output_has_a_residual_floor() {
        let echo = room(&signal(13));
        let zero = vec![0.0; echo.len()];
        let raw = metrics(&zero, &echo, &echo, 0);
        let clean = metrics(&zero, &echo, &zero, 0);
        assert!(raw["residual_reduction_db"].as_f64().unwrap().abs() < 1e-10);
        assert!(clean["residual_reduction_db"].as_f64().unwrap() > 80.0);
    }
}
