//! Deterministic signal-level AEC comparison. No capture, network, models or user audio.
use fotw_pipeline::echo::{EchoGate, GateVerdict};
use sonora::{AudioProcessing, Config, StreamConfig, config::EchoCanceller};
use std::time::Instant;

const RATE: usize = 16000;
const FRAME: usize = RATE / 100;
const SAMPLES: usize = RATE * 20;
const WARMUP: usize = RATE * 8;

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
            let mut aec = AudioProcessing::builder()
                .config(Config {
                    echo_canceller: Some(EchoCanceller {
                        enforce_high_pass_filtering: false,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .capture_config(StreamConfig::new(RATE as u32, 1))
                .render_config(StreamConfig::new(RATE as u32, 1))
                .build();
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
        _ => panic!("unknown evaluation backend"),
    }
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.max(1e-12).log10()
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
    for backend in ["passthrough", "gate-100ms", "sonora-0.2.0-aec3"] {
        let began = Instant::now();
        let control = run(backend, &zero, &near);
        let wall_ms = began.elapsed().as_secs_f64() * 1000.0;
        let lag = delay(&near, &control);
        results.push(serde_json::json!({"backend":backend, "case":"near-only",
            "metrics":metrics(&near, &zero, &control, lag), "wall_ms":wall_ms}));
        for (case, clean_near) in [("echo-only", &zero), ("double-talk", &near)] {
            let mic: Vec<_> = clean_near.iter().zip(&echo).map(|(n, e)| n + e).collect();
            let began = Instant::now();
            let out = run(backend, &far, &mic);
            let wall_ms = began.elapsed().as_secs_f64() * 1000.0;
            results.push(serde_json::json!({"backend":backend,"case":case,
                "metrics":metrics(clean_near, &echo, &out, lag), "wall_ms":wall_ms}));
        }
    }
    println!(
        "{}",
        serde_json::json!({"synthetic_only":true, "hardware":hardware,
        "sample_rate_hz":RATE, "audio_seconds_per_case":20,"warmup_seconds":8,
        "ns_agc_disabled":true,"candidate_selected_for_app":false,
        "fixed_frame_probe_only":true,"backend_tail_drained":false,"results":results})
    );
}

#[cfg(test)]
mod tests {
    use super::*;
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
