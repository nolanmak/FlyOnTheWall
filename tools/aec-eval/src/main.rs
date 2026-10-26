//! AEC comparison with generated signals or opt-in approved public speech.
#[cfg(feature = "native-webrtc")]
mod clocked;
mod quality;
#[cfg(all(target_os = "macos", feature = "speech-retention"))]
mod speech;
#[cfg(any(all(target_os = "macos", feature = "speech-retention"), test))]
mod word_error;
use fotw_pipeline::aec::{Adapter, Backend, Config as AdapterConfig, OutputKind};
use fotw_pipeline::echo::{EchoGate, GateVerdict};
use sonora::{AudioProcessing, Config, StreamConfig, config::EchoCanceller};
use std::time::Instant;

const RATE: usize = 16000;
const FRAME: usize = RATE / 100;
const SAMPLES: usize = RATE * 20;
const WARMUP: usize = RATE * 8;
#[cfg(feature = "native-webrtc")]
const CONSERVATIVE_ENR_SCALE: f32 = 16.0;
#[cfg(feature = "native-webrtc")]
const CONSERVATIVE_FILTER_BLOCKS: usize = 32;

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

#[cfg(any(all(target_os = "macos", feature = "speech-retention"), test))]
fn room(far: &[f32]) -> Vec<f32> {
    room_response(far, &[(640, 0.6), (1120, 0.25), (1560, 0.1)], 0.0)
}

fn room_response(far: &[f32], taps: &[(usize, f32)], drift_ppm: f64) -> Vec<f32> {
    (0..far.len())
        .map(|i| {
            taps.iter()
                .map(|(delay, gain)| {
                    if drift_ppm == 0.0 {
                        return i.checked_sub(*delay).map_or(0.0, |j| far[j] * gain);
                    }
                    let position = i as f64 * (1.0 + drift_ppm / 1_000_000.0) - *delay as f64;
                    if position < 0.0 {
                        return 0.0;
                    }
                    let at = position.floor() as usize;
                    let fraction = (position - at as f64) as f32;
                    let a = far.get(at).copied().unwrap_or(0.0);
                    let b = far.get(at + 1).copied().unwrap_or(0.0);
                    (a + fraction * (b - a)) * gain
                })
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
        #[cfg(feature = "native-webrtc")]
        "native-webrtc-2.1.0-conservative" => run_native_profile(far, mic, true),
        _ => panic!("unknown evaluation backend"),
    }
}

#[cfg(feature = "native-webrtc")]
fn run_native(far: &[f32], mic: &[f32]) -> Vec<f32> {
    run_native_profile(far, mic, false)
}

#[cfg(feature = "native-webrtc")]
fn native_processor(conservative: bool) -> webrtc_audio_processing::Processor {
    use webrtc_audio_processing::{Config, Processor, config::EchoCanceller};
    let processor = if conservative {
        let mut config = webrtc_audio_processing::experimental::EchoCanceller3Config::default();
        // 32 * 64 samples covers 128 ms of the echo path at 16 kHz.
        config.filter.refined.length_blocks = CONSERVATIVE_FILTER_BLOCKS;
        config.filter.coarse.length_blocks = CONSERVATIVE_FILTER_BLOCKS;
        config.filter.refined_initial.length_blocks = CONSERVATIVE_FILTER_BLOCKS;
        config.filter.coarse_initial.length_blocks = CONSERVATIVE_FILTER_BLOCKS;
        let suppressor = &mut config.suppressor;
        for tuning in [
            &mut suppressor.normal_tuning,
            &mut suppressor.nearend_tuning,
        ] {
            for mask in [&mut tuning.mask_lf, &mut tuning.mask_hf] {
                mask.enr_transparent *= CONSERVATIVE_ENR_SCALE;
                mask.enr_suppress *= CONSERVATIVE_ENR_SCALE;
            }
        }
        assert!(
            config.validate(),
            "conservative parameters must be accepted unchanged"
        );
        Processor::with_aec3_config(RATE as u32, config).unwrap()
    } else {
        Processor::new(RATE as u32).unwrap()
    };
    assert_eq!(processor.num_samples_per_frame(), FRAME);
    processor.set_config(Config {
        echo_canceller: Some(EchoCanceller::Full {
            stream_delay_ms: None,
        }),
        ..Default::default()
    });
    processor
}

#[cfg(feature = "native-webrtc")]
fn run_native_profile(far: &[f32], mic: &[f32], conservative: bool) -> Vec<f32> {
    assert_eq!(far.len(), mic.len());
    assert!(
        mic.len().is_multiple_of(FRAME),
        "fixed-frame control cannot drop a tail"
    );
    let processor = native_processor(conservative);
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

fn hardware() -> Option<String> {
    if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    } else {
        None
    }
}

fn quality_report(results: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let value = |backend: &str, case: &str, metric: &str| {
        results
            .iter()
            .find(|r| r["backend"] == backend && r["case"] == case)
            .and_then(|r| r["metrics"][metric].as_f64())
            .unwrap_or(f64::NAN)
    };
    let raw_snr = value("passthrough", "double-talk", "near_snr_db");
    results
        .iter()
        .filter(|r| r["case"] == "near-only")
        .map(|r| {
            let backend = r["backend"].as_str().unwrap();
            let gates = quality::assess(
                value(backend, "near-only", "near_gain_db"),
                value(backend, "echo-only", "residual_reduction_db"),
                value(backend, "double-talk", "near_gain_db"),
                value(backend, "double-talk", "near_snr_db"),
                raw_snr,
            );
            serde_json::json!({"backend":backend,"echo_reduction":gates.echo_reduction,
            "near_only":gates.near_only,"overlap_gain":gates.overlap_gain,
            "overlap_snr":gates.overlap_snr,"signal_gates_pass":gates.pass(),
            "app_backend_selected":false})
        })
        .collect()
}

fn aggregate_quality(fixtures: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let backends: std::collections::BTreeSet<_> = fixtures
        .iter()
        .filter_map(|f| f["signal_qualification"].as_array())
        .flatten()
        .filter_map(|q| q["backend"].as_str())
        .collect();
    backends.into_iter().map(|backend| {
        let pass = fixtures.iter().all(|f| f["signal_qualification"].as_array().is_some_and(|rows|
            rows.iter().any(|q| q["backend"] == backend && q["signal_gates_pass"] == true)));
        serde_json::json!({"backend":backend,"all_fixture_signal_gates_pass":pass,"app_backend_selected":false})
    }).collect()
}

fn native_profile_description() -> Option<serde_json::Value> {
    #[cfg(feature = "native-webrtc")]
    {
        Some(
            serde_json::json!({"backend":"native-webrtc-2.1.0-conservative",
        "experimental_aec3_api":true,"filter_length_blocks":CONSERVATIVE_FILTER_BLOCKS,
        "filter_block_samples":64,"enr_threshold_scale":CONSERVATIVE_ENR_SCALE,
        "emr_thresholds_changed":false,"clock_drift_resampling":false}),
        )
    }
    #[cfg(not(feature = "native-webrtc"))]
    {
        None
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !args.is_empty() {
        #[cfg(feature = "native-webrtc")]
        if args.len() == 1 && args[0] == "--clocked-signals" {
            println!("{}", clocked::probe());
            return Ok(());
        }
        if args.len() == 1 && args[0] == "--heldout-signals" {
            let fixtures = vec![
                signal_probe(
                    "original",
                    127,
                    915,
                    &[(640, 0.6), (1120, 0.25), (1560, 0.1)],
                    0.0,
                ),
                signal_probe(
                    "unseen-short",
                    621,
                    8901,
                    &[(96, 0.5), (512, 0.24), (1408, 0.08)],
                    0.0,
                ),
                signal_probe(
                    "unseen-long",
                    7311,
                    8932,
                    &[(224, 0.45), (992, 0.25), (2016, 0.14)],
                    0.0,
                ),
                signal_probe(
                    "unseen-drift",
                    217,
                    501,
                    &[(416, 0.55), (1216, 0.2), (1904, 0.1)],
                    80.0,
                ),
            ];
            let aggregate = aggregate_quality(&fixtures);
            println!(
                "{}",
                serde_json::json!({"synthetic_only":true,"fixtures":fixtures,"aggregate":aggregate,
                "candidate_selected_for_app":false,"route_reset_and_callback_drift_qualified":false})
            );
            return Ok(());
        }
        #[cfg(all(target_os = "macos", feature = "speech-retention"))]
        if args[0] == "--speech-retention" {
            return speech::run(&args[1..]);
        }
        return Err("use no arguments or --heldout-signals for generated signals; --clocked-signals requires native-webrtc; --speech-retention WAV MODEL requires speech-retention on macOS".into());
    }
    println!(
        "{}",
        signal_probe(
            "original",
            127,
            915,
            &[(640, 0.6), (1120, 0.25), (1560, 0.1)],
            0.0
        )
    );
    Ok(())
}

fn signal_probe(
    label: &str,
    far_seed: u32,
    near_seed: u32,
    taps: &[(usize, f32)],
    drift_ppm: f64,
) -> serde_json::Value {
    let far = signal(far_seed);
    let near = signal(near_seed);
    let echo = room_response(&far, taps, drift_ppm);
    let zero = vec![0.0; SAMPLES];
    let hardware = hardware();
    let mut results = Vec::new();
    let backends = [
        "passthrough",
        "gate-100ms",
        "sonora-0.2.0-aec3",
        "sonora-0.2.0-adapted",
        #[cfg(feature = "native-webrtc")]
        "native-webrtc-2.1.0-aec3",
        #[cfg(feature = "native-webrtc")]
        "native-webrtc-2.1.0-conservative",
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
    let qualification = quality_report(&results);
    serde_json::json!({"synthetic_only":true, "hardware":hardware,
        "fixture":label,"far_seed":far_seed,"near_seed":near_seed,"room_taps":taps,"drift_ppm":drift_ppm,
        "sample_rate_hz":RATE, "audio_seconds_per_case":20,"warmup_seconds":8,
        "ns_agc_disabled":true,"candidate_selected_for_app":false,
        "native_conservative_parameters":native_profile_description(),
        "native_webrtc_evaluated":cfg!(feature = "native-webrtc"),
        "undrained_controls_present":true,"callback_adapter_evaluated":true,
        "signal_qualification":qualification,"results":results})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_passing_calibration_cannot_hide_a_failed_or_missing_validation_fixture() {
        let pass = serde_json::json!({"signal_qualification":[{"backend":"candidate","signal_gates_pass":true}]});
        let fail = serde_json::json!({"signal_qualification":[{"backend":"candidate","signal_gates_pass":false}]});
        let missing = serde_json::json!({"signal_qualification":[]});
        for validation in [fail, missing.clone()] {
            assert_eq!(
                aggregate_quality(&[pass.clone(), validation])[0]["all_fixture_signal_gates_pass"],
                false
            );
        }
        assert_eq!(
            aggregate_quality(&[missing, pass.clone()])[0]["all_fixture_signal_gates_pass"],
            false
        );
        assert_eq!(
            aggregate_quality(&[pass.clone(), pass])[0]["all_fixture_signal_gates_pass"],
            true
        );
    }
    #[test]
    fn generated_room_preserves_integer_delays_and_interpolates_drift_without_changing_duration() {
        let source = [0.0, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(
            room_response(&source, &[(1, 0.5)], 0.0),
            [0.0, 0.0, 0.5, 1.0, 1.5]
        );
        assert_eq!(
            room_response(&source, &[(1, 0.5)], 500000.0),
            [0.0, 0.25, 1.0, 1.75, 0.0]
        );
    }
    #[cfg(feature = "native-webrtc")]
    #[test]
    fn conservative_profile_meets_the_original_signal_gates_without_changing_them() {
        let far = signal(127);
        let near = signal(915);
        let echo = room(&far);
        let zero = vec![0.0; SAMPLES];
        let backend = "native-webrtc-2.1.0-conservative";
        let control = run(backend, &zero, &near);
        let lag = delay(&near, &control);
        let near_metrics = metrics(&near, &zero, &control, lag);
        let echo_metrics = metrics(&zero, &echo, &run(backend, &far, &echo), lag);
        let mic: Vec<_> = near.iter().zip(&echo).map(|(n, e)| n + e).collect();
        let overlap = metrics(&near, &echo, &run(backend, &far, &mic), lag);
        let raw = metrics(&near, &echo, &mic, 0);
        let gates = quality::assess(
            near_metrics["near_gain_db"].as_f64().unwrap(),
            echo_metrics["residual_reduction_db"].as_f64().unwrap(),
            overlap["near_gain_db"].as_f64().unwrap(),
            overlap["near_snr_db"].as_f64().unwrap(),
            raw["near_snr_db"].as_f64().unwrap(),
        );
        assert!(
            gates.pass(),
            "profile lost signal preservation: {gates:?}; near {near_metrics}, echo {echo_metrics}, overlap {overlap}, raw {raw}"
        );
    }
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
