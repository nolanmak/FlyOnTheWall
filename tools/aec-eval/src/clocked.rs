//! Evaluation-only interpolation driven by synthetic native clock observations.
//! The processor receives observations, never the fixture's declared ppm.
use super::*;
use fotw_pipeline::{
    reference_clock::{Estimate, Tracker},
    reference_resample::{
        AlignmentEpoch, Config as ReferenceConfig, FrameDomain, HostNs, NativeFrame,
        ProcessingFrame, ReferenceBuffer, Request, TapGeneration,
    },
    ring::CaptureObservation,
};

fn trace(ppm: f64, fault: bool) -> Vec<CaptureObservation> {
    (0..SAMPLES / FRAME)
        .map(|i| {
            let frame = i * FRAME;
            CaptureObservation {
                sequence: i as u64 + 1,
                sample_start: frame as u64,
                sample_count: FRAME,
                device_frames: frame as u64,
                host_ns: (frame as f64 / (RATE as f64 * (1.0 + ppm / 1e6)) * 1e9).round() as u64,
                flags: if fault && i == 1000 { 4 } else { 0 },
                dropped_samples: 0,
                timing_faults: u64::from(fault && i >= 1000),
            }
        })
        .collect()
}

fn reference(
    far: &[f32],
    render: &Estimate,
    mic: &Estimate,
    at: usize,
    now: u64,
) -> Option<[f32; FRAME]> {
    let mut aligned = [0.0; FRAME];
    for (offset, sample) in aligned.iter_mut().enumerate() {
        let p = render.map_frame_from(mic, (at + offset) as u64, now)?;
        let index = usize::try_from(p.frame).ok()?;
        let a = *far.get(index)?;
        let b = if p.fraction == 0.0 {
            a
        } else {
            *far.get(index.checked_add(1)?)?
        };
        *sample = a + (b - a) * p.fraction as f32;
    }
    Some(aligned)
}

struct NativeBackend {
    processor: webrtc_audio_processing::Processor,
}
impl Backend for NativeBackend {
    type Error = ();
    fn frame_samples(&self) -> usize {
        self.processor.num_samples_per_frame()
    }
    fn delay_samples(&self) -> usize {
        128
    }
    fn analyze_render(&mut self, frame: &[f32]) -> Result<(), ()> {
        self.processor
            .analyze_render_frame([frame])
            .map(|_| ())
            .map_err(|_| ())
    }
    fn process_capture(&mut self, frame: &[f32], output: &mut [f32]) -> Result<(), ()> {
        output.copy_from_slice(frame);
        self.processor
            .process_capture_frame([output])
            .map(|_| ())
            .map_err(|_| ())
    }
    fn reset(&mut self) -> Result<(), ()> {
        self.processor = native_processor(true);
        Ok(())
    }
}

fn run_clocked(
    far: &[f32],
    mic: &[f32],
    render_trace: &[CaptureObservation],
    mic_trace: &[CaptureObservation],
) -> (Vec<f32>, serde_json::Value) {
    run_with_reference(far, mic, render_trace, mic_trace, false, false)
}

fn run_with_reference(
    far: &[f32],
    mic: &[f32],
    render_trace: &[CaptureObservation],
    mic_trace: &[CaptureObservation],
    bandlimited: bool,
    timestamp_recovery: bool,
) -> (Vec<f32>, serde_json::Value) {
    let reference_config = ReferenceConfig {
        render_rate_hz: RATE as u32,
        capacity_frames: RATE / 4,
        mic: FrameDomain {
            native_origin: NativeFrame(0),
            native_offset_frames: 0.0,
            processing_origin: ProcessingFrame(0),
            native_rate_hz: RATE as u32,
        },
        render_generation: TapGeneration(1),
        mic_generation: TapGeneration(1),
        epoch: AlignmentEpoch(1),
    };
    let mut history = bandlimited.then(|| ReferenceBuffer::new(reference_config).unwrap());
    let (mut loaded, mut max_retained) = (0, 0);
    let mut epoch = 1;
    let tracker = || {
        if timestamp_recovery {
            Tracker::with_timestamp_recovery(RATE as u32, 1).unwrap()
        } else {
            Tracker::new(RATE as u32, 1).unwrap()
        }
    };
    let mut render_clock = tracker();
    let mut mic_clock = tracker();
    assert_eq!(far.len(), mic.len());
    assert!(mic.len().is_multiple_of(FRAME));
    assert_eq!(render_trace.len(), mic.len() / FRAME);
    assert_eq!(render_trace.len(), mic_trace.len());
    let mut adapter = Adapter::new(
        NativeBackend {
            processor: native_processor(true),
        },
        AdapterConfig {
            frame_samples: FRAME,
            delay_samples: 128,
            max_wait_samples: FRAME * 2,
            max_reference_samples: FRAME * 4,
        },
        1,
        0,
    )
    .unwrap();
    let mut blocks = Vec::new();
    let mut bypass = 0;
    let mut rates = None;
    for (i, capture) in mic.chunks_exact(FRAME).enumerate() {
        // Eight-callback observation lead, plus enough arrival budget for the
        // whole latest callback to have been captured before processing.
        let index = (i + 8).min(render_trace.len() - 1);
        let (r, m) = (render_trace[index], mic_trace[index]);
        let now = r.host_ns.max(m.host_ns) + 20_000_000 + ((index % 7) as u64 + 1) * 1_000_000;
        let render = render_clock.observe(1, r, now);
        let microphone = mic_clock.observe(1, m, now);
        if let Some(history) = history.as_mut() {
            if render.is_err() || microphone.is_err() {
                let old_epoch = epoch;
                epoch += 1;
                history
                    .reset(
                        AlignmentEpoch(epoch),
                        TapGeneration(1),
                        TapGeneration(1),
                        reference_config.mic,
                    )
                    .unwrap();
                assert_eq!(
                    history.append(
                        AlignmentEpoch(old_epoch),
                        TapGeneration(1),
                        NativeFrame(loaded as u64),
                        &[]
                    ),
                    Err(fotw_pipeline::reference_resample::Error::Epoch)
                );
                adapter
                    .reset(epoch, (i * FRAME) as u64, &mut blocks)
                    .unwrap();
            }
            let end = (index + 1) * FRAME;
            if end > loaded {
                history
                    .append(
                        AlignmentEpoch(epoch),
                        TapGeneration(1),
                        NativeFrame(loaded as u64),
                        &far[loaded..end],
                    )
                    .unwrap();
                loaded = end;
            }
            max_retained = max_retained.max(history.retained_frames());
        }
        let render = render.ok().flatten();
        let microphone = microphone.ok().flatten();
        let aligned = render.zip(microphone).and_then(|(r, m)| {
            rates = Some((r.rate_hz, m.rate_hz));
            if let Some(history) = history.as_mut() {
                let mut aligned = [0.0; FRAME];
                history
                    .fill(
                        Request {
                            epoch: AlignmentEpoch(epoch),
                            start: ProcessingFrame((i * FRAME) as u64),
                            now: HostNs(now),
                            render: r,
                            mic: m,
                        },
                        &mut aligned,
                    )
                    .ok()?;
                Some(aligned)
            } else {
                reference(far, &r, &m, i * FRAME, now)
            }
        });
        if let Some(aligned) = aligned {
            adapter
                .push_render(epoch, (i * FRAME) as u64, &aligned, &mut blocks)
                .unwrap();
        } else {
            bypass += 1;
        }
        adapter
            .push_capture(epoch, (i * FRAME) as u64, capture, &mut blocks)
            .unwrap();
        if aligned.is_none() {
            adapter.flush_reference_wait(&mut blocks);
        }
        assert!(adapter.buffered_samples() <= 128 + FRAME * 7);
    }
    adapter.finish(&mut blocks);
    let stats = adapter.stats();
    let mut output = Vec::with_capacity(mic.len());
    let mut last_epoch = 1;
    for block in blocks {
        assert_eq!(block.start_sample, output.len() as u64);
        assert!(block.epoch >= last_epoch && block.epoch <= epoch);
        last_epoch = block.epoch;
        if matches!(block.kind, OutputKind::Raw(_)) {
            assert_eq!(
                block.samples,
                mic[output.len()..output.len() + block.samples.len()],
                "fallback must retain original samples exactly"
            );
        }
        output.extend(block.samples);
    }
    assert_eq!(output.len(), mic.len());
    (
        output,
        serde_json::json!({"render_rate_hz":rates.map(|r|r.0),"mic_rate_hz":rates.map(|r|r.1),
        "missing_reference_frames":bypass,"raw_passthrough_samples":stats.raw_samples,"processed_samples":stats.cleaned_samples,
        "backend_failures":stats.backend_failures,"backend_tail_drained":true,"callback_adapter":true,
        "observation_lead_ms":80,"minimum_acquisition_delivery_ms":20,"linear_interpolation_evaluation_only":!bandlimited,
        "bandlimited_reference":bandlimited,"isolated_timestamp_recovery":timestamp_recovery,"maximum_retained_native_reference_frames":max_retained,"reference_history_resets":epoch-1}),
    )
}

pub(super) fn probe() -> serde_json::Value {
    probe_with_reference(false)
}
pub(super) fn bandlimited_probe() -> serde_json::Value {
    probe_with_reference(true)
}
fn probe_with_reference(bandlimited: bool) -> serde_json::Value {
    let mut fixtures = Vec::new();
    for (label, ppm, fault) in [
        ("nominal", 0.0, false),
        ("positive-drift", 80.0, false),
        ("negative-drift", -120.0, false),
        ("timing-fault", 80.0, true),
    ] {
        let far = signal(217);
        let near = signal(501);
        let zero = vec![0.0; SAMPLES];
        let echo = room_response(&far, &[(416, 0.55), (1216, 0.2), (1904, 0.1)], ppm);
        let r = trace(ppm, fault);
        let m = trace(0.0, false);
        let mut results = Vec::new();
        let mut backends = vec![
            "passthrough",
            "native-webrtc-2.1.0-conservative",
            "native-webrtc-2.1.0-clocked-eval",
        ];
        if bandlimited {
            backends.push("native-webrtc-2.1.0-bandlimited-eval");
            backends.push("native-webrtc-2.1.0-recovery-eval");
        }
        for backend in backends {
            let mut lag = 0;
            for (case, clean, render) in [
                ("near-only", &near, &zero),
                ("echo-only", &zero, &far),
                ("double-talk", &near, &far),
            ] {
                let mic: Vec<f32> = if case == "near-only" {
                    near.clone()
                } else {
                    clean.iter().zip(&echo).map(|(n, e)| n + e).collect()
                };
                let start = Instant::now();
                let (out, clocks) = if backend == "passthrough" {
                    (mic.clone(), serde_json::Value::Null)
                } else if backend.ends_with("recovery-eval") {
                    run_with_reference(render, &mic, &r, &m, true, true)
                } else if backend.ends_with("bandlimited-eval") {
                    run_with_reference(render, &mic, &r, &m, true, false)
                } else if backend.ends_with("clocked-eval") {
                    run_clocked(render, &mic, &r, &m)
                } else {
                    (
                        run_native_profile(render, &mic, true),
                        serde_json::Value::Null,
                    )
                };
                if case == "near-only" {
                    lag = delay(&near, &out);
                }
                results.push(serde_json::json!({"backend":backend,"case":case,"metrics":metrics(clean,if case=="near-only" {&zero}else{&echo},&out,lag),
                    "clocks":clocks,"wall_ms":start.elapsed().as_secs_f64()*1000.0}));
            }
        }
        fixtures.push(serde_json::json!({"fixture":label,"declared_fixture_ppm":ppm,"injected_timing_fault":fault,
            "signal_qualification":quality_report(&results),"results":results}));
    }
    serde_json::json!({"synthetic_only":true,"hardware":hardware(),"aggregate":aggregate_quality(&fixtures),"fixtures":fixtures,
        "sample_rate_hz":RATE,"audio_seconds_per_case":SAMPLES/RATE,"warmup_seconds":WARMUP/RATE,"metric_tail_excluded_samples":FRAME*2,
        "candidate_selected_for_app":false,"bandlimited_resampling_qualified":false,"physical_clock_alignment_qualified":false,
        "bandlimited_reference_evaluated":bandlimited,
        "native_reference_diagnostics":if bandlimited {Some(crate::reference_probe::run())}else{None},
        "native_conservative_parameters":native_profile_description()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_worker_compensates_delay_and_drains_a_partial_stop_on_the_original_clock() {
        use fotw_pipeline::aec_worker::{Config, Worker};
        use std::time::{Duration, Instant};
        let mut worker = Worker::new(
            NativeBackend {
                processor: native_processor(true),
            },
            Config {
                adapter: AdapterConfig {
                    frame_samples: FRAME,
                    delay_samples: 128,
                    max_wait_samples: 320,
                    max_reference_samples: 640,
                },
                queued_blocks: 2,
                block_samples: 320,
                pending_samples: 1024,
                deadline_ms: 1000,
            },
            7,
            73,
        )
        .unwrap();
        let near = signal(501)[..16159].to_vec();
        let mut blocks = Vec::new();
        let mut start = 73;
        let deadline = Instant::now() + Duration::from_secs(10);
        for chunk in near.chunks(320) {
            worker
                .submit(
                    7,
                    start,
                    chunk,
                    Some(&vec![0.0; chunk.len()]),
                    0,
                    &mut blocks,
                )
                .unwrap();
            start += chunk.len() as u64;
            while worker.pending_samples() >= 288 {
                worker.poll(0, &mut blocks).unwrap();
                assert!(Instant::now() < deadline, "native worker stalled");
                std::thread::yield_now();
            }
        }
        worker.finish(0, &mut blocks).unwrap();
        while worker.status().is_running() || worker.pending_samples() > 0 {
            worker.poll(0, &mut blocks).unwrap();
            assert!(
                Instant::now() < deadline,
                "native worker did not release its tail/resource"
            );
            std::thread::yield_now();
        }
        let mut output = Vec::new();
        for block in blocks {
            assert_eq!(
                (block.epoch, block.start_sample),
                (7, 73 + output.len() as u64)
            );
            assert_eq!(block.kind, OutputKind::Cleaned);
            output.extend(block.samples);
        }
        assert_eq!(output.len(), near.len());
        // The whole-probe lag estimator starts after eight seconds and cannot
        // measure this short tail fixture. Compare every unshifted sample.
        let worst = output
            .iter()
            .zip(near)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(
            worst < 1e-5,
            "native worker changed/delayed original samples: {worst}"
        );
    }
    #[test]
    fn bounded_bandlimited_reference_keeps_continuous_gates_and_exposes_fault_failure() {
        let report = bandlimited_probe();
        let diagnostics = &report["native_reference_diagnostics"];
        for row in diagnostics["aliases"].as_array().unwrap() {
            if row["input_tone_hz"].as_f64().unwrap() >= 8500.0 {
                assert_eq!(row["alias_gate_pass"], true);
            }
        }
        for row in diagnostics["native_conversion_alignment"]
            .as_array()
            .unwrap()
        {
            assert_eq!(row["phase_gate_pass"], true);
        }
        for f in report["fixtures"].as_array().unwrap() {
            let recovery = f["signal_qualification"]
                .as_array()
                .unwrap()
                .iter()
                .find(|q| q["backend"] == "native-webrtc-2.1.0-recovery-eval")
                .unwrap();
            assert_eq!(recovery["app_backend_selected"], false);
            assert_eq!(recovery["near_only"], true);
            assert_eq!(recovery["echo_reduction"], true);
            assert_eq!(
                recovery["signal_gates_pass"],
                f["injected_timing_fault"] == false,
                "isolated clock recovery does not qualify post-reset overlap"
            );
            if f["injected_timing_fault"] == true {
                assert_eq!(recovery["overlap_gain"], false);
                assert_eq!(recovery["overlap_snr"], false);
            }
            let q = f["signal_qualification"]
                .as_array()
                .unwrap()
                .iter()
                .find(|q| q["backend"] == "native-webrtc-2.1.0-bandlimited-eval")
                .unwrap();
            assert_eq!(
                q["signal_gates_pass"],
                f["injected_timing_fault"] == false,
                "{}",
                f["fixture"]
            );
            assert_eq!(q["near_only"], true);
            for row in f["results"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["backend"] == "native-webrtc-2.1.0-bandlimited-eval")
            {
                assert_eq!(row["clocks"]["bandlimited_reference"], true);
                assert_eq!(
                    row["clocks"]["maximum_retained_native_reference_frames"],
                    4000
                );
                assert_eq!(
                    row["clocks"]["reference_history_resets"],
                    if f["injected_timing_fault"] == true {
                        1
                    } else {
                        0
                    }
                );
            }
        }
    }
    #[test]
    fn bandlimited_history_fences_faults_and_retains_the_original_near_clock() {
        let near = signal(501);
        let zero = vec![0.0; SAMPLES];
        let (out, stats) = run_with_reference(
            &zero,
            &near,
            &trace(80.0, true),
            &trace(0.0, false),
            true,
            false,
        );
        assert_eq!(stats["reference_history_resets"], 1);
        assert!(
            stats["maximum_retained_native_reference_frames"]
                .as_u64()
                .unwrap()
                <= 4000
        );
        assert_eq!(out.len(), near.len());
        assert_eq!(delay(&near, &out), 0);
        let error = out
            .iter()
            .zip(&near)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(error < 1e-5);
    }
    #[test]
    fn continuous_clock_quality_passes_but_fault_quality_is_not_hidden() {
        let report = probe();
        for f in report["fixtures"].as_array().unwrap() {
            let quality = f["signal_qualification"]
                .as_array()
                .unwrap()
                .iter()
                .find(|q| q["backend"] == "native-webrtc-2.1.0-clocked-eval")
                .unwrap();
            if f["injected_timing_fault"] == true {
                assert_eq!(quality["signal_gates_pass"], false);
                assert_eq!(quality["near_only"], true);
            } else {
                assert_eq!(
                    quality["signal_gates_pass"], true,
                    "continuous clock fixture {} must meet the unchanged gates",
                    f["fixture"]
                );
            }
        }
        let aggregate = report["aggregate"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["backend"] == "native-webrtc-2.1.0-clocked-eval")
            .unwrap();
        assert_eq!(aggregate["all_fixture_signal_gates_pass"], false);
    }
    #[test]
    fn timing_fault_preserves_every_near_sample_on_the_original_clock() {
        let near = signal(501);
        let zero = vec![0.0; SAMPLES];
        let (out, _) = run_clocked(&zero, &near, &trace(80.0, true), &trace(0.0, false));
        assert_eq!(out.len(), near.len());
        assert_eq!(
            delay(&near, &out),
            0,
            "clock/reset worker must compensate native delay"
        );
        let worst = out
            .iter()
            .zip(&near)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(
            worst < 1e-5,
            "near-only clock/reset path changed a sample by {worst}"
        );
    }
    #[test]
    fn reference_uses_estimated_clocks_instead_of_equal_sample_indexes() {
        let mut r = Tracker::new(RATE as u32, 1).unwrap();
        let mut m = Tracker::new(RATE as u32, 1).unwrap();
        let (rt, mt) = (trace(80.0, false), trace(0.0, false));
        let (mut rf, mut mf) = (None, None);
        for i in 0..400 {
            rf = r.observe(1, rt[i], mt[i].host_ns + 1_000_000).unwrap();
            mf = m.observe(1, mt[i], mt[i].host_ns + 1_000_000).unwrap();
        }
        let far: Vec<f32> = (0..SAMPLES).map(|i| i as f32 / 1_000_000.0).collect();
        let aligned = reference(&far, &rf.unwrap(), &mf.unwrap(), 48000, 4_000_000_000).unwrap();
        assert!(
            (aligned[0] - 0.04800384).abs() < 1e-8,
            "drifting clocks must not use the same buffer indexes"
        );
    }
}
