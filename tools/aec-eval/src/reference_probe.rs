//! Generated native-rate/phase diagnostics; no model, device or provider.
use fotw_pipeline::{
    reference_clock::{Estimate, Tracker},
    reference_resample::*,
    resample::ArchiveConverter,
    ring::CaptureObservation,
};
use std::f64::consts::TAU;

fn clock(rate: u32, channels: u16, ppm: f64) -> Estimate {
    let mut tracker = Tracker::new(rate, channels).unwrap();
    let mut fit = None;
    for i in 0..40_u64 {
        let frame = i * u64::from(rate / 10);
        let o = CaptureObservation {
            sequence: i * 10 + 1,
            sample_start: frame * u64::from(channels),
            sample_count: rate as usize / 100 * usize::from(channels),
            device_frames: frame,
            host_ns: (frame as f64 / (f64::from(rate) * (1.0 + ppm / 1e6)) * 1e9).round() as u64,
            flags: 0,
            dropped_samples: 0,
            timing_faults: 0,
        };
        fit = tracker.observe(1, o, o.host_ns + 20_000_000).unwrap();
    }
    fit.unwrap()
}
fn signal(rate: u32, ppm: f64, start: usize, len: usize, hz: f64) -> Vec<f32> {
    (start..start + len)
        .map(|i| (TAU * hz * i as f64 / (f64::from(rate) * (1.0 + ppm / 1e6))).sin() as f32 * 0.5)
        .collect()
}
fn config(rate: u32, mic: u32) -> Config {
    Config {
        render_rate_hz: rate,
        capacity_frames: rate as usize / 2,
        mic: FrameDomain {
            native_origin: NativeFrame(0),
            native_offset_frames: 0.0,
            processing_origin: ProcessingFrame(0),
            native_rate_hz: mic,
        },
        render_generation: TapGeneration(1),
        mic_generation: TapGeneration(1),
        epoch: AlignmentEpoch(1),
    }
}
fn rms(pcm: &[f32]) -> f64 {
    (pcm.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
}

pub(super) fn run() -> serde_json::Value {
    let mut aliases = Vec::new();
    for rate in [44100, 48000, 96000, 192000, 768000] {
        let mut buffer = ReferenceBuffer::new(config(rate, 16000)).unwrap();
        for (n, hz) in [1000.0, 8500.0, 9000.0, 15000.0].into_iter().enumerate() {
            let epoch = AlignmentEpoch(n as u64 + 1);
            if n > 0 {
                buffer
                    .reset(
                        epoch,
                        TapGeneration(1),
                        TapGeneration(1),
                        config(rate, 16000).mic,
                    )
                    .unwrap();
            }
            let data = signal(rate, 0.0, rate as usize * 3, rate as usize / 4, hz);
            buffer
                .append(
                    epoch,
                    TapGeneration(1),
                    NativeFrame(u64::from(rate) * 3),
                    &data,
                )
                .unwrap();
            let mut out = [0.0; 160];
            buffer
                .fill(
                    Request {
                        epoch,
                        start: ProcessingFrame(49000),
                        now: HostNs(4_000_000_000),
                        render: clock(rate, 2, 0.0),
                        mic: clock(16000, 1, 0.0),
                    },
                    &mut out,
                )
                .unwrap();
            let ratio_db = 20.0 * (rms(&out) / (0.5 / 2.0_f64.sqrt())).max(1e-15).log10();
            aliases.push(serde_json::json!({"native_render_rate_hz":rate,"input_tone_hz":hz,"output_rate_hz":16000,"amplitude_db":ratio_db,"retained_native_frames":buffer.retained_frames(),"alias_gate_pass":(hz>=8500.0).then_some(ratio_db< -65.0)}));
        }
    }
    let mut conversions = Vec::new();
    for mic_rate in [44100, 44160, 48000] {
        let mut converter = ArchiveConverter::new(mic_rate, 1, 16000, 1).unwrap();
        let origin = converter.output_origin();
        let mic = signal(mic_rate, -120.0, 0, mic_rate as usize * 4, 997.0);
        let mut canonical = converter.process(&mic).unwrap();
        canonical.extend(converter.finish().unwrap());
        let far = signal(48000, 80.0, 144000, 12000, 997.0);
        let mut errors = Vec::new();
        for phase in [
            0.0,
            origin.input_frame_numerator as f64 / origin.denominator as f64,
        ] {
            let mut cfg = config(48000, mic_rate);
            cfg.mic.native_offset_frames = phase;
            let mut buffer = ReferenceBuffer::new(cfg).unwrap();
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
                .fill(
                    Request {
                        epoch: AlignmentEpoch(1),
                        start: ProcessingFrame(49000),
                        now: HostNs(4_000_000_000),
                        render: clock(48000, 2, 80.0),
                        mic: clock(mic_rate, 1, -120.0),
                    },
                    &mut out,
                )
                .unwrap();
            let error: Vec<_> = out
                .iter()
                .zip(&canonical[49000..50600])
                .map(|(a, b)| a - b)
                .collect();
            errors.push(rms(&error));
        }
        conversions.push(serde_json::json!({"native_render_rate_hz":48000,"native_render_channels":2,"render_mono_pcm_supplied":true,"render_fixture_ppm":80.0,"native_mic_rate_hz":mic_rate,"mic_fixture_ppm":-120.0,
            "origin_numerator":origin.input_frame_numerator,"origin_denominator":origin.denominator,"zero_origin_rms_error":errors[0],"reported_origin_rms_error":errors[1],"processing_frames_checked":1600,"phase_gate_pass":errors[1]<0.002}));
    }
    serde_json::json!({"synthetic_only":true,"aliases":aliases,"native_conversion_alignment":conversions,"physical_acceptance":false})
}
