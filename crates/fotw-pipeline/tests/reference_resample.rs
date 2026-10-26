use fotw_pipeline::rt::{RtAlloc, RtGuard, allocation_violations};
use fotw_pipeline::{
    reference_clock::{Estimate, Tracker},
    reference_resample::*,
    resample::ArchiveConverter,
    ring::CaptureObservation,
};
use std::f64::consts::TAU;
#[global_allocator]
static ALLOC: RtAlloc = RtAlloc;

fn estimate(rate: u32, channels: u16, generation: u64) -> Estimate {
    let mut tracker = Tracker::new(rate, channels).unwrap();
    let mut fit = None;
    for i in 0..40_u64 {
        let frames = i * u64::from(rate / 10);
        let o = CaptureObservation {
            sequence: i * 10 + 1,
            sample_start: frames * u64::from(channels),
            sample_count: rate as usize / 100 * usize::from(channels),
            device_frames: frames,
            host_ns: i * 100_000_000,
            flags: 0,
            dropped_samples: 0,
            timing_faults: 0,
        };
        fit = tracker
            .observe(generation, o, o.host_ns + 20_000_000)
            .unwrap();
    }
    fit.unwrap()
}
fn config(rate: u32, mic_rate: u32) -> Config {
    Config {
        render_rate_hz: rate,
        capacity_frames: rate as usize / 2,
        mic: FrameDomain {
            native_origin: NativeFrame(0),
            native_offset_frames: 0.0,
            processing_origin: ProcessingFrame(0),
            native_rate_hz: mic_rate,
        },
        render_generation: TapGeneration(1),
        mic_generation: TapGeneration(1),
        epoch: AlignmentEpoch(1),
    }
}
fn request(rate: u32, mic_rate: u32, start: u64) -> Request {
    Request {
        epoch: AlignmentEpoch(1),
        start: ProcessingFrame(start),
        now: HostNs(4_000_000_000),
        render: estimate(rate, 2, 1),
        mic: estimate(mic_rate, 1, 1),
    }
}
fn sine(rate: u32, start: usize, n: usize, hz: f64) -> Vec<f32> {
    (start..start + n)
        .map(|i| (TAU * hz * i as f64 / f64::from(rate)).sin() as f32 * 0.5)
        .collect()
}
fn rms(pcm: &[f32]) -> f64 {
    (pcm.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
}

#[test]
fn native_48k_reference_rejects_aliases_without_changing_the_microphone_clock() {
    for rate in [44100, 48000, 96000, 192000, 768000] {
        for hz in [1000.0, 8500.0, 9000.0, 15000.0] {
            let mut buffer = ReferenceBuffer::new(config(rate, 44100)).unwrap();
            let data = sine(rate, rate as usize * 3, rate as usize / 4, hz);
            buffer
                .append(
                    AlignmentEpoch(1),
                    TapGeneration(1),
                    NativeFrame(u64::from(rate) * 3),
                    &data,
                )
                .unwrap();
            let mut out = [0.0; 160];
            buffer.fill(request(rate, 44100, 49_000), &mut out).unwrap();
            if hz == 1000.0 {
                let expected = sine(16000, 49000, 160, hz);
                let error: Vec<_> = out.iter().zip(expected).map(|(a, b)| a - b).collect();
                assert!(rms(&error) < 1e-4, "passband/phase error {}", rms(&error));
            } else {
                let attenuation = 20.0 * (rms(&out) / (0.5 / 2.0_f64.sqrt())).log10();
                assert!(
                    attenuation < -65.0,
                    "{rate}Hz capture/{hz}Hz alias only suppressed {attenuation}dB"
                );
            }
        }
    }
}

#[test]
fn history_is_bounded_and_callback_splits_do_not_change_output() {
    let mut a = ReferenceBuffer::new(config(48000, 48000)).unwrap();
    let mut b = ReferenceBuffer::new(config(48000, 48000)).unwrap();
    let data = sine(48000, 120000, 48000, 733.0);
    a.append(
        AlignmentEpoch(1),
        TapGeneration(1),
        NativeFrame(120000),
        &data,
    )
    .unwrap();
    let mut at = 0;
    let mut block = 0;
    while at < data.len() {
        let n = [37, 512, 29, 997][block % 4].min(data.len() - at);
        b.append(
            AlignmentEpoch(1),
            TapGeneration(1),
            NativeFrame((120000 + at) as u64),
            &data[at..at + n],
        )
        .unwrap();
        at += n;
        block += 1;
        assert!(b.retained_frames() <= 24000);
    }
    let mut x = [0.0; 160];
    let mut y = [0.0; 160];
    let req = request(48000, 48000, 51000);
    a.fill(req, &mut x).unwrap();
    b.fill(req, &mut y).unwrap();
    assert_eq!(x, y);
}

#[test]
fn missing_support_invalid_writes_and_stale_work_cannot_publish_partial_output() {
    let mut buffer = ReferenceBuffer::new(config(48000, 44100)).unwrap();
    let data = sine(48000, 144000, 12000, 300.0);
    buffer
        .append(
            AlignmentEpoch(1),
            TapGeneration(1),
            NativeFrame(144000),
            &data,
        )
        .unwrap();
    let retained = buffer.retained_frames();
    assert_eq!(
        buffer.append(
            AlignmentEpoch(1),
            TapGeneration(1),
            NativeFrame(156000),
            &[0.25, f32::NAN]
        ),
        Err(Error::Samples)
    );
    assert_eq!(buffer.retained_frames(), retained);
    assert_eq!(
        buffer.append(
            AlignmentEpoch(1),
            TapGeneration(1),
            NativeFrame(156001),
            &[0.0]
        ),
        Err(Error::Positions)
    );
    let mut out = [123.0; 160];
    assert_eq!(
        buffer.fill(request(48000, 44100, 51900), &mut out),
        Err(Error::MissingReference)
    );
    assert_eq!(out, [123.0; 160]);
    let mut stale = request(48000, 44100, 49000);
    stale.now = HostNs(6_000_000_000);
    assert_eq!(buffer.fill(stale, &mut out), Err(Error::Clock));
    assert_eq!(out, [123.0; 160]);
    buffer
        .reset(
            AlignmentEpoch(2),
            TapGeneration(1),
            TapGeneration(2),
            config(48000, 44100).mic,
        )
        .unwrap();
    assert_eq!(buffer.retained_frames(), 0);
    assert_eq!(
        buffer.append(
            AlignmentEpoch(1),
            TapGeneration(1),
            NativeFrame(156000),
            &[0.0]
        ),
        Err(Error::Epoch)
    );
    assert_eq!(
        buffer.fill(request(48000, 44100, 49000), &mut out),
        Err(Error::Epoch)
    );
    let mut wrong_generation = request(48000, 44100, 49000);
    wrong_generation.epoch = AlignmentEpoch(2);
    assert_eq!(
        buffer.fill(wrong_generation, &mut out),
        Err(Error::Generation)
    );
    assert_eq!(out, [123.0; 160]);
}

#[test]
fn invalid_budgets_domains_and_oversized_blocks_are_refused_before_publication() {
    for capacity in [0, usize::MAX] {
        let mut cfg = config(48000, 44100);
        cfg.capacity_frames = capacity;
        assert!(matches!(ReferenceBuffer::new(cfg), Err(Error::Config)));
    }
    let mut cfg = config(48000, 44100);
    cfg.mic.native_offset_frames = f64::NAN;
    assert!(matches!(ReferenceBuffer::new(cfg), Err(Error::Config)));
    let mut buffer = ReferenceBuffer::new(config(48000, 44100)).unwrap();
    assert_eq!(
        buffer.append(
            AlignmentEpoch(1),
            TapGeneration(1),
            NativeFrame(u64::MAX),
            &[0.0]
        ),
        Err(Error::Positions)
    );
    assert_eq!(buffer.retained_frames(), 0);
    let input = vec![0.0; 48001];
    assert_eq!(
        buffer.append(AlignmentEpoch(1), TapGeneration(1), NativeFrame(0), &input),
        Err(Error::Block)
    );
    assert_eq!(buffer.retained_frames(), 0);
    let mut out = [123.0; 3201];
    assert_eq!(
        buffer.fill(request(48000, 44100, 49000), &mut out),
        Err(Error::Block)
    );
    assert!(out.iter().all(|v| *v == 123.0));
}

#[test]
fn native_reference_agrees_with_the_existing_microphone_conversion_clock() {
    for rate in [44100, 44160, 48000] {
        let mut converted = ArchiveConverter::new(rate, 1, 16000, 1).unwrap();
        let origin = converted.output_origin();
        let mic = sine(rate, 0, rate as usize * 4, 300.0);
        let mut canonical = converted.process(&mic).unwrap();
        canonical.extend(converted.finish().unwrap());
        assert_eq!(
            converted.output_origin(),
            origin,
            "geometry must not depend on how much trim remains"
        );
        let mut cfg = config(48000, rate);
        cfg.mic.native_offset_frames =
            origin.input_frame_numerator as f64 / origin.denominator as f64;
        let mut buffer = ReferenceBuffer::new(cfg).unwrap();
        let far = sine(48000, 144000, 12000, 300.0);
        buffer
            .append(
                AlignmentEpoch(1),
                TapGeneration(1),
                NativeFrame(144000),
                &far,
            )
            .unwrap();
        let mut out = [0.0; 1600];
        buffer.fill(request(48000, rate, 49000), &mut out).unwrap();
        let error: Vec<_> = out
            .iter()
            .zip(&canonical[49000..50600])
            .map(|(a, b)| a - b)
            .collect();
        assert!(
            rms(&error) < 0.002,
            "conversion origin/phase mismatch {}",
            rms(&error)
        );
    }
}

#[test]
fn preallocated_append_lookup_and_rejection_paths_do_not_allocate() {
    let mut buffer = ReferenceBuffer::new(config(48000, 44100)).unwrap();
    let data = sine(48000, 144000, 12000, 300.0);
    let req = request(48000, 44100, 49000);
    let mut out = [0.0; 160];
    let before = allocation_violations();
    {
        let _guard = RtGuard::enter();
        buffer
            .append(
                AlignmentEpoch(1),
                TapGeneration(1),
                NativeFrame(144000),
                &data,
            )
            .unwrap();
        buffer.fill(req, &mut out).unwrap();
        assert_eq!(
            buffer.append(
                AlignmentEpoch(0),
                TapGeneration(1),
                NativeFrame(156000),
                &[0.0]
            ),
            Err(Error::Epoch)
        );
    }
    assert_eq!(allocation_violations(), before);
}

#[test]
fn capture_normalization_and_processing_phases_compose_without_rewriting_pcm() {
    for (native, archive) in [
        (44100, 48000),
        (44160, 48000),
        (48000, 44100),
        (44160, 44100),
    ] {
        let mut capture = ArchiveConverter::new(native, 1, archive, 1).unwrap();
        let mut processing = ArchiveConverter::new(archive, 1, 16000, 1).unwrap();
        let a = capture.output_origin();
        let b = processing.output_origin();
        let phase = a.input_frame_numerator as f64 / a.denominator as f64
            + b.input_frame_numerator as f64 / b.denominator as f64 * f64::from(native)
                / f64::from(archive);
        let pcm = sine(native, 0, native as usize * 4, 997.0);
        let mut raw = capture.process(&pcm).unwrap();
        raw.extend(capture.finish().unwrap());
        let mut canonical = processing.process(&raw).unwrap();
        canonical.extend(processing.finish().unwrap());
        assert_eq!(canonical.len(), 64000);
        let mut cfg = config(48000, native);
        cfg.mic.native_offset_frames = phase;
        let mut buffer = ReferenceBuffer::new(cfg).unwrap();
        let far = sine(48000, 144000, 12000, 997.0);
        buffer
            .append(
                AlignmentEpoch(1),
                TapGeneration(1),
                NativeFrame(144000),
                &far,
            )
            .unwrap();
        let mut out = [0.0; 1600];
        buffer
            .fill(request(48000, native, 49000), &mut out)
            .unwrap();
        let error: Vec<_> = out
            .iter()
            .zip(&canonical[49000..50600])
            .map(|(a, b)| a - b)
            .collect();
        assert!(
            rms(&error) < 0.002,
            "{native}->{archive}->16000 phase mismatch {}",
            rms(&error)
        );
    }
}

#[test]
fn explicit_origins_preserve_positions_above_floating_point_integer_precision() {
    let base = (1_u64 << 54) + 3;
    let host_base = 1_u64 << 60;
    let clock = |rate: u32| {
        let mut tracker = Tracker::new(rate, 1).unwrap();
        let mut fit = None;
        for i in 0..40_u64 {
            let frames = i * u64::from(rate / 10);
            let o = CaptureObservation {
                sequence: i * 10 + 1,
                sample_start: base + frames,
                sample_count: rate as usize / 100,
                device_frames: base + frames,
                host_ns: host_base + i * 100_000_000,
                flags: 0,
                dropped_samples: 0,
                timing_faults: 0,
            };
            fit = tracker.observe(1, o, o.host_ns + 20_000_000).unwrap();
        }
        fit.unwrap()
    };
    let mut cfg = config(48000, 44100);
    cfg.mic.native_origin = NativeFrame(base);
    cfg.mic.processing_origin = ProcessingFrame(base);
    let mut buffer = ReferenceBuffer::new(cfg).unwrap();
    let data = sine(48000, 144000, 12000, 733.0);
    buffer
        .append(
            AlignmentEpoch(1),
            TapGeneration(1),
            NativeFrame(base + 144000),
            &data,
        )
        .unwrap();
    let mut out = [0.0; 160];
    buffer
        .fill(
            Request {
                epoch: AlignmentEpoch(1),
                start: ProcessingFrame(base + 49000),
                now: HostNs(host_base + 4_000_000_000),
                render: clock(48000),
                mic: clock(44100),
            },
            &mut out,
        )
        .unwrap();
    let expected = sine(16000, 49000, 160, 733.0);
    let error: Vec<_> = out.iter().zip(expected).map(|(a, b)| a - b).collect();
    assert!(
        rms(&error) < 1e-4,
        "large origin lost frame precision: {}",
        rms(&error)
    );
}
