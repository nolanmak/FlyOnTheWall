//! The microphone leg.
//!
//! Deliberately a **separate device and a separate IOProc** from the system
//! tap, never a single aggregate containing both. Merging them is the most
//! commonly reported failure in this area: the two devices have independent
//! clocks, and a stale or mis-clocked aggregate silently produces drift or
//! stops delivering. They also change independently — the user can switch
//! output to AirPods without touching the input device, and a fused aggregate
//! has to be torn down for either event.
//!
//! Keeping them apart is also what makes "me vs them" attribution free: two
//! physically separate streams need no diarization to tell the local speaker
//! from everyone else (seam rule 2). The two are reconciled after the fact
//! through `host_ns`, which both legs stamp from the same monotonic clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cidre::{blocks, cat, core_audio::System, dispatch};

use super::capture_assets::NativeAssets;
use super::capture_lifecycle::{CaptureOwner, Failure};
use crate::clock;
use crate::error::TapError;
use crate::format::{SampleFormat, StreamFormat};
use crate::frames::{CaptureTimestamp, FrameFlags, FrameSink};
use crate::ids::TapId;

struct IoState {
    sink: Mutex<Option<Box<dyn FrameSink>>>,
    device_frames: AtomicU64,
}

/// A microphone capture stream.
pub struct MicTap {
    id: TapId,
    hint: StreamFormat,
    running: Option<Running>,
    cleanup_failure: Option<Failure>,
    state: Arc<IoState>,
}

struct Running {
    format: StreamFormat,
    capture: CaptureOwner<NativeAssets>,
}

impl std::fmt::Debug for MicTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicTap")
            .field("id", &self.id)
            .field("running", &self.running.is_some())
            .finish()
    }
}

impl MicTap {
    /// Prepare a tap on the default input device.
    pub fn default_input() -> Result<Self, TapError> {
        let device = System::default_input_device()
            .map_err(|e| TapError::platform(format!("no default input device: {e:?}")))?;
        let uid = device
            .uid()
            .map(|u| u.to_string())
            .unwrap_or_else(|_| "default".to_string());
        Ok(Self {
            id: TapId::mic(uid),
            hint: StreamFormat::new(48_000, 1, SampleFormat::F32),
            running: None,
            cleanup_failure: None,
            state: Arc::new(IoState {
                sink: Mutex::new(None),
                device_frames: AtomicU64::new(0),
            }),
        })
    }

    fn build(&mut self, sink: Box<dyn FrameSink>) -> Result<Running, TapError> {
        let device = System::default_input_device()
            .map_err(|e| TapError::platform(format!("no default input device: {e:?}")))?;

        // Read the format every time. A Bluetooth headset engaging HFP drops
        // the input to 16 or 24 kHz mid-session, and a converter configured
        // once at session start would then be silently wrong (CAP-07).
        // Live rate-change notification/rebuild remains separate from probing.
        let asbd = device.input_asbd().map_err(|e| {
            TapError::platform(format!("Cannot read microphone stream format: {e:?}"))
        })?;
        let format = capture_format(&asbd)?;

        self.state.device_frames.store(0, Ordering::Release);
        {
            let mut guard = self
                .state
                .sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *guard = Some(sink);
        }

        let queue = dispatch::Queue::serial_with_ar_pool();
        let state = Arc::clone(&self.state);
        let channels = u64::from(format.channels.max(1));

        let block = blocks::EscBlock::new5(
            move |_now: &cat::AudioTimeStamp,
                  input: &cat::AudioBufList<1>,
                  _input_time: &cat::AudioTimeStamp,
                  _output: &mut cat::AudioBufList<1>,
                  _output_time: &cat::AudioTimeStamp| {
                on_io(&state, input, channels);
            },
        );

        let capture = CaptureOwner::start(NativeAssets::mic(device, block, queue))?;
        Ok(Running { format, capture })
    }
}

fn on_io(state: &IoState, input: &cat::AudioBufList<1>, channels: u64) {
    let Ok(mut guard) = state.sink.try_lock() else {
        return;
    };
    let Some(sink) = guard.as_mut() else {
        return;
    };
    // The ASBD was validated before starting. Never reinterpret a changed
    // buffer layout using the old channel count, including partial frames.
    // A typed error does not count as a delivered buffer in watchdog health.
    let bad_layout = input.number_buffers != 1
        || channels == 0
        || u64::from(input.buffers[0].number_channels) != channels
        || !u64::from(input.buffers[0].data_bytes_size).is_multiple_of(channels * 4);
    if bad_layout {
        sink.on_error(TapError::StreamFormatChanged);
        return;
    }

    let buf = &input.buffers[0];
    if buf.data.is_null() || buf.data_bytes_size == 0 {
        return;
    }
    let samples = buf.data_bytes_size as usize / std::mem::size_of::<f32>();
    // SAFETY: Core Audio guarantees `data` is `data_bytes_size` bytes of
    // interleaved f32, valid for this callback only. We copy and do not retain.
    let pcm = unsafe { std::slice::from_raw_parts(buf.data as *const f32, samples) };

    let frames = samples as u64 / channels.max(1);
    let before = state.device_frames.fetch_add(frames, Ordering::AcqRel);

    let mut flags = FrameFlags::empty();
    flags.set(FrameFlags::SILENT, pcm.iter().all(|s| *s == 0.0));

    sink.on_frames(pcm, CaptureTimestamp::new(before, clock::host_ns()), flags);
}

impl Drop for MicTap {
    fn drop(&mut self) {
        // Revoke the application sink even when callers rely on Drop alone.
        let _ = crate::tap::AudioTap::stop(self);
    }
}

impl crate::tap::AudioTap for MicTap {
    fn id(&self) -> &TapId {
        &self.id
    }

    fn format(&self) -> StreamFormat {
        self.running.as_ref().map_or(self.hint, |r| r.format)
    }

    fn format_is_authoritative(&self) -> bool {
        self.running.is_some()
    }

    fn start(&mut self, sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        NativeAssets::check_mic()?;
        if self.running.is_some() {
            return Err(TapError::platform("mic tap is already started"));
        }
        // Initialize the capture epoch/timebase on the control thread before
        // native startup can invoke the real-time callback.
        let _ = clock::host_ns();
        if let Some(failure) = &self.cleanup_failure {
            return Err(failure.error());
        }
        match self.build(sink) {
            Ok(running) => {
                let format = running.format;
                self.running = Some(running);
                Ok(format)
            }
            Err(error) => {
                self.cleanup_failure = Failure::from_error(&error);
                self.state
                    .sink
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                Err(error)
            }
        }
    }

    fn stop(&mut self) -> Result<(), TapError> {
        self.stop_observed(&mut || {})
    }
    fn stop_observed(&mut self, stopped: &mut dyn FnMut()) -> Result<(), TapError> {
        // Revoke application delivery and finish any admitted sink call before
        // entering native Stop/unregistration. Retained failed callbacks see None.
        self.state
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(failure) = &self.cleanup_failure {
            if failure.state.capture_stopped() {
                stopped();
            }
            return Err(failure.error());
        }
        let result = match self.running.take() {
            Some(mut running) => running.capture.close_observed(stopped),
            None => {
                stopped();
                Ok(())
            }
        };
        if let Err(error) = &result {
            self.cleanup_failure = Failure::from_error(error);
        }
        result
    }
}

fn capture_format(asbd: &cat::AudioStreamBasicDesc) -> Result<StreamFormat, TapError> {
    let channels = u16::try_from(asbd.channels_per_frame)
        .map_err(|_| TapError::FormatUnsupported("Microphone channel count is too large".into()))?;
    let format = StreamFormat::new(asbd.sample_rate as u32, channels, SampleFormat::F32);
    let bytes_per_frame = u32::from(channels) * 4;
    if !asbd.sample_rate.is_finite()
        || asbd.sample_rate.fract() != 0.0
        || !format.is_plausible()
        || !asbd.is_native_endian()
        || !asbd
            .format_flags
            .contains(cat::audio::FormatFlags::NATIVE_FLOAT_PACKED)
        || asbd.bits_per_channel != 32
        || asbd.frames_per_packet != 1
        || asbd.bytes_per_frame != bytes_per_frame
        || asbd.bytes_per_packet != bytes_per_frame
        || (!asbd.is_interleaved() && channels != 1)
    {
        return Err(TapError::FormatUnsupported("Microphone must provide a known-rate, native packed Float32 interleaved stream (or mono Float32)".into()));
    }
    Ok(format)
}

#[cfg(test)]
mod format_tests {
    use super::*;
    #[derive(Default)]
    struct Observed {
        samples: Vec<f32>,
        flags: Vec<FrameFlags>,
        timestamps: Vec<CaptureTimestamp>,
        format_errors: usize,
    }
    struct FixtureSink(Arc<Mutex<Observed>>);
    impl FrameSink for FixtureSink {
        fn on_frames(&mut self, pcm: &[f32], at: CaptureTimestamp, flags: FrameFlags) {
            let mut seen = self.0.lock().unwrap();
            seen.samples.extend_from_slice(pcm);
            seen.flags.push(flags);
            seen.timestamps.push(at);
        }
        fn on_error(&mut self, error: TapError) {
            assert!(matches!(error, TapError::StreamFormatChanged));
            assert!(error.is_transient_device_fault());
            self.0.lock().unwrap().format_errors += 1;
        }
    }
    #[test]
    fn changed_channel_layout_is_not_delivered_under_the_old_format() {
        let seen = Arc::new(Mutex::new(Observed::default()));
        let state = IoState {
            sink: Mutex::new(Some(Box::new(FixtureSink(seen.clone())))),
            device_frames: AtomicU64::new(0),
        };
        let mut pcm = [0.25_f32, -0.25, 0.5, -0.5];
        let mut input = cat::AudioBufList {
            number_buffers: 1,
            buffers: [cat::AudioBuf {
                number_channels: 2,
                data_bytes_size: 16,
                data: pcm.as_mut_ptr().cast(),
            }],
        };
        on_io(&state, &input, 2);
        assert_eq!(state.device_frames.load(Ordering::Acquire), 2);
        input.buffers[0].number_channels = 1;
        on_io(&state, &input, 2);
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.samples, pcm,
            "changed layout must not append audio under stale metadata"
        );
        assert_eq!(state.device_frames.load(Ordering::Acquire), 2);
        assert_eq!(seen.format_errors, 1);
        assert_eq!(
            seen.flags.len(),
            1,
            "a rejected buffer must not refresh watchdog health"
        );
    }
    #[test]
    fn preserves_stereo_rate_and_frame_clock() {
        let actual =
            capture_format(&cat::AudioStreamBasicDesc::common_f32(24000.0, 2, true)).unwrap();
        assert_eq!(actual, StreamFormat::new(24000, 2, SampleFormat::F32));
        assert_eq!(actual.frames_in(480), Some(240));
        assert_eq!(actual.frames_to_ns(240), 10_000_000);
    }
    #[test]
    fn refuses_unknown_or_incompatible_descriptions_instead_of_guessing() {
        let valid = cat::AudioStreamBasicDesc::common_f32(48000.0, 1, true);
        let mut cases = Vec::new();
        for rate in [0.0, f64::NAN, f64::INFINITY, 48000.5] {
            let mut bad = valid;
            bad.sample_rate = rate;
            cases.push(bad);
        }
        let mut integer = valid;
        integer.bits_per_channel = 16;
        integer.bytes_per_frame = 2;
        cases.push(integer);
        let mut truncated = valid;
        truncated.bytes_per_packet = 3;
        cases.push(truncated);
        let mut unknown = valid;
        unknown.channels_per_frame = 0;
        cases.push(unknown);
        for bad in cases {
            assert!(capture_format(&bad).is_err(), "accepted {bad:?}");
        }
        assert!(capture_format(&cat::AudioStreamBasicDesc::common_f32(16000.0, 1, false)).is_ok());
    }
}
