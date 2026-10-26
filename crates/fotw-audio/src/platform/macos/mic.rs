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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cidre::{blocks, cat, core_audio::System, dispatch};

use super::capture_assets::NativeAssets;
use super::capture_clock::CaptureSink;
use super::capture_lifecycle::{CaptureOwner, Failure};
use crate::clock;
use crate::error::TapError;
use crate::format::{SampleFormat, StreamFormat};
#[cfg(any(test, feature = "conformance"))]
use crate::frames::CaptureTimestamp;
use crate::frames::{FrameFlags, FrameSink};
use crate::ids::TapId;

/// Read-only input metadata. This is not proof of permission or live audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSnapshot {
    /// Current physical input UID, suitable for comparing readiness observations.
    pub device_uid: String,
    /// Format validated by the same contract used before native capture starts.
    pub format: StreamFormat,
}

/// Inspect the default input without creating a tap, listener or IOProc.
///
/// Core Audio property reads are synchronous and may block. Callers must own
/// and bound their worker, and must revalidate at capture start. Two matching
/// reads establish only an observation, never sustained health or a route lock.
pub fn default_input_snapshot() -> Result<InputSnapshot, TapError> {
    snapshot_with(|| {
        let device = System::default_input_device()
            .map_err(|e| TapError::platform(format!("no default input device: {e:?}")))?;
        if device.0.0 == 0 {
            return Err(TapError::DeviceNotFound(crate::DeviceId::new(
                "default input",
            )));
        }
        let uid = device
            .uid()
            .map_err(|e| TapError::platform(format!("cannot read input identity: {e:?}")))?;
        let asbd = device
            .input_asbd()
            .map_err(|e| TapError::platform(format!("cannot read input format: {e:?}")))?;
        Ok((device.0.0, uid.to_string(), asbd))
    })
}

fn snapshot_with(
    mut read: impl FnMut() -> Result<(u32, String, cat::AudioStreamBasicDesc), TapError>,
) -> Result<InputSnapshot, TapError> {
    let mut observed = || {
        let (object, device_uid, asbd) = read()?;
        if object == 0 || device_uid.trim().is_empty() {
            return Err(TapError::DeviceNotFound(crate::DeviceId::new(
                "default input",
            )));
        }
        Ok((
            object,
            InputSnapshot {
                device_uid,
                format: capture_format(&asbd)?,
            },
        ))
    };
    let first = observed()?;
    let second = observed()?;
    if first != second {
        return Err(TapError::StreamFormatChanged);
    }
    Ok(second.1)
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    fn raw(
        id: u32,
        uid: &str,
        rate: f64,
        channels: u32,
    ) -> (u32, String, cat::AudioStreamBasicDesc) {
        (
            id,
            uid.into(),
            cat::AudioStreamBasicDesc::common_f32(rate, channels, true),
        )
    }
    #[test]
    fn readiness_rejects_flapping_identity_and_format_without_starting_capture() {
        for second in [
            raw(2, "input-a", 48000.0, 1),
            raw(1, "input-b", 48000.0, 1),
            raw(1, "input-a", 24000.0, 1),
            raw(1, "input-a", 48000.0, 2),
        ] {
            let mut reads = vec![raw(1, "input-a", 48000.0, 1), second].into_iter();
            assert!(
                snapshot_with(|| Ok(reads.next().unwrap())).is_err(),
                "a changing route cannot report ready"
            );
        }
    }
    #[test]
    fn readiness_rejects_unknown_identity_and_uses_capture_format_validation() {
        for (id, uid, rate, channels) in [
            (0, "input", 48000.0, 1),
            (1, "", 48000.0, 1),
            (1, "input", f64::NAN, 1),
            (1, "input", 48000.0, 0),
        ] {
            assert!(snapshot_with(|| Ok(raw(id, uid, rate, channels))).is_err());
        }
        let mut count = 0;
        let snapshot = snapshot_with(|| {
            count += 1;
            Ok(raw(1, "input", 24000.0, 2))
        })
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(snapshot.device_uid, "input");
        assert_eq!(
            snapshot.format,
            StreamFormat::new(24000, 2, SampleFormat::F32)
        );
    }
    #[test]
    fn readiness_does_not_hide_a_native_read_error() {
        let mut reads = 0;
        let result = snapshot_with(|| {
            reads += 1;
            if reads == 1 {
                Ok(raw(1, "input", 48000.0, 1))
            } else {
                Err(TapError::DeviceNotFound(crate::DeviceId::new(
                    "fixture disappeared",
                )))
            }
        });
        assert!(matches!(result, Err(TapError::DeviceNotFound(_))));
    }
}

struct IoState {
    sink: Mutex<Option<CaptureSink>>,
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
    format_changed: Arc<AtomicBool>,
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

        let changed = Arc::new(AtomicBool::new(false));
        let (owner, format) = CaptureOwner::prepare(NativeAssets::mic(device), |assets| {
            assets.watch_mic_format(changed.clone())?;
            // Listeners precede the format read. A topology/rate change during
            // preparation invalidates this generation before native Start.
            let asbd = device.input_asbd().map_err(|e| {
                TapError::platform(format!("Cannot read microphone stream format: {e:?}"))
            })?;
            let format = capture_format(&asbd)?;
            self.state.device_frames.store(0, Ordering::Release);
            *self
                .state
                .sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(CaptureSink::new(sink));
            let queue = dispatch::Queue::serial_with_ar_pool();
            let state = self.state.clone();
            let observed = changed.clone();
            let channels = u64::from(format.channels.max(1));
            let block = blocks::EscBlock::new5(
                move |_now: &cat::AudioTimeStamp,
                      input: &cat::AudioBufList<1>,
                      input_time: &cat::AudioTimeStamp,
                      _output: &mut cat::AudioBufList<1>,
                      _output_time: &cat::AudioTimeStamp| {
                    on_io(&state, input, channels, input_time, &observed);
                },
            );
            assets.set_callback(block, queue);
            if changed.load(Ordering::Acquire) {
                return Err(TapError::StreamFormatChanged);
            }
            Ok(format)
        })?;
        let capture = owner.start_prepared()?;
        Ok(Running {
            format,
            capture,
            format_changed: changed,
        })
    }
}

fn on_io(
    state: &IoState,
    input: &cat::AudioBufList<1>,
    channels: u64,
    input_time: &cat::AudioTimeStamp,
    changed: &AtomicBool,
) {
    let Ok(mut guard) = state.sink.try_lock() else {
        return;
    };
    let Some(sink) = guard.as_mut() else {
        return;
    };
    if changed.load(Ordering::Acquire) {
        sink.on_error(TapError::StreamFormatChanged);
        return;
    }
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

    sink.on_input(pcm, input_time, before, frames, flags);
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
        self.running
            .as_ref()
            .is_some_and(|running| !running.format_changed.load(Ordering::Acquire))
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
            Some(mut running) => {
                let closed = running.capture.close_observed(stopped);
                // Listener removal/queue drain precedes this observation, so a
                // format notification with no subsequent IO still fails Stop.
                format_result(running.format_changed.load(Ordering::Acquire), closed)
            }
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

fn format_result(changed: bool, result: Result<(), TapError>) -> Result<(), TapError> {
    result?;
    if changed {
        Err(TapError::FormatChangedAfterStop)
    } else {
        Ok(())
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

// Run the actual native-block -> atomic -> IO guard path without a device.
#[cfg(any(test, feature = "conformance"))]
pub(super) fn input_format_contract() {
    use std::sync::atomic::AtomicUsize;
    struct Counters {
        samples: Arc<AtomicUsize>,
        errors: Arc<AtomicUsize>,
    }
    impl FrameSink for Counters {
        fn on_frames(&mut self, pcm: &[f32], _: CaptureTimestamp, _: FrameFlags) {
            self.samples.fetch_add(pcm.len(), Ordering::AcqRel);
        }
        fn on_error(&mut self, error: TapError) {
            assert!(matches!(error, TapError::StreamFormatChanged));
            self.errors.fetch_add(1, Ordering::AcqRel);
        }
    }
    let changed = Arc::new(AtomicBool::new(false));
    let mut watch = super::input_format::InputFormatWatch::new(changed.clone());
    let samples = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let state = IoState {
        sink: Mutex::new(Some(CaptureSink::new(Box::new(Counters {
            samples: samples.clone(),
            errors: errors.clone(),
        })))),
        device_frames: AtomicU64::new(0),
    };
    let mut pcm = [0.25_f32, -0.25];
    let input = cat::AudioBufList {
        number_buffers: 1,
        buffers: [cat::AudioBuf {
            number_channels: 1,
            data_bytes_size: 8,
            data: pcm.as_mut_ptr().cast(),
        }],
    };
    on_io(&state, &input, 1, &cat::AudioTimeStamp::invalid(), &changed);
    assert_eq!(samples.load(Ordering::Acquire), 2);
    watch.notify_for_conformance();
    on_io(&state, &input, 1, &cat::AudioTimeStamp::invalid(), &changed);
    assert_eq!(
        samples.load(Ordering::Acquire),
        2,
        "rate-only change must not append samples"
    );
    assert_eq!(state.device_frames.load(Ordering::Acquire), 2);
    assert_eq!(errors.load(Ordering::Acquire), 1);
    assert!(matches!(
        format_result(changed.load(Ordering::Acquire), Ok(())),
        Err(TapError::FormatChangedAfterStop)
    ));
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
    #[ignore = "requires an installed default input; registers metadata listeners only, never starts capture"]
    fn installed_input_listener_registration_without_capture() {
        let device = System::default_input_device().expect("installed default input is required");
        assert_ne!(
            device.0.0, 0,
            "no default input available; physical listener probe is unperformed"
        );
        let (mut owner, ()) = CaptureOwner::prepare(NativeAssets::mic(device), |assets| {
            assets.watch_mic_format(Arc::new(AtomicBool::new(false)))
        })
        .expect("input rate/configuration/stream listeners must register");
        // Deliberately never call start_prepared: this probes metadata listener
        // registration/removal without opening an IOProc or collecting audio.
        owner
            .close()
            .expect("all input listeners must unregister and drain");
    }

    #[test]
    fn known_invalidated_format_is_not_reported_as_authoritative() {
        let changed = Arc::new(AtomicBool::new(false));
        let (capture, ()) = CaptureOwner::prepare(
            NativeAssets::mic(cidre::core_audio::Device(cidre::core_audio::Obj(77))),
            |_| Ok(()),
        )
        .unwrap();
        let mut tap = MicTap {
            id: TapId::mic("fixture"),
            hint: StreamFormat::new(16000, 1, SampleFormat::F32),
            running: Some(Running {
                format: StreamFormat::new(16000, 1, SampleFormat::F32),
                capture,
                format_changed: changed.clone(),
            }),
            cleanup_failure: None,
            state: Arc::new(IoState {
                sink: Mutex::new(None),
                device_frames: AtomicU64::new(0),
            }),
        };
        assert!(crate::AudioTap::format_is_authoritative(&tap));
        changed.store(true, Ordering::Release);
        assert!(
            !crate::AudioTap::format_is_authoritative(&tap),
            "known stale input format cannot be authoritative"
        );
        assert!(matches!(
            crate::AudioTap::stop(&mut tap),
            Err(TapError::FormatChangedAfterStop)
        ));
    }
    #[test]
    fn finalized_format_invalidation_preserves_positive_release_evidence_for_next_attempt() {
        let error = format_result(true, Ok(())).unwrap_err();
        assert_eq!(
            error.cleanup_state(),
            Some(crate::CleanupState::Released),
            "a format fault after full cleanup must not permanently fence the next dictation"
        );
    }
    #[test]
    fn format_change_without_further_io_fails_finalization_but_preserves_cleanup_failure() {
        assert!(matches!(
            format_result(true, Ok(())),
            Err(TapError::FormatChangedAfterStop)
        ));
        assert!(format_result(false, Ok(())).is_ok());
        let cleanup = TapError::Cleanup {
            state: crate::CleanupState::Stopped,
            message: "fixture retained listener".into(),
        };
        assert!(matches!(
            format_result(true, Err(cleanup)),
            Err(TapError::Cleanup {
                state: crate::CleanupState::Stopped,
                ..
            })
        ));
    }
    #[test]
    fn rate_invalidation_rejects_an_unchanged_buffer_layout_before_advancing_the_clock() {
        let seen = Arc::new(Mutex::new(Observed::default()));
        let state = IoState {
            sink: Mutex::new(Some(CaptureSink::new(Box::new(FixtureSink(seen.clone()))))),
            device_frames: AtomicU64::new(0),
        };
        let mut pcm = [0.25_f32, -0.25];
        let input = cat::AudioBufList {
            number_buffers: 1,
            buffers: [cat::AudioBuf {
                number_channels: 1,
                data_bytes_size: 8,
                data: pcm.as_mut_ptr().cast(),
            }],
        };
        on_io(
            &state,
            &input,
            1,
            &cat::AudioTimeStamp::invalid(),
            &AtomicBool::new(true),
        );
        assert!(
            seen.lock().unwrap().samples.is_empty(),
            "changed rate must not be delivered using stale ASBD"
        );
        assert_eq!(state.device_frames.load(Ordering::Acquire), 0);
        assert_eq!(seen.lock().unwrap().format_errors, 1);
    }
    #[test]
    fn missing_native_time_is_flagged_without_losing_input_audio() {
        let seen = Arc::new(Mutex::new(Observed::default()));
        let state = IoState {
            sink: Mutex::new(Some(CaptureSink::new(Box::new(FixtureSink(seen.clone()))))),
            device_frames: AtomicU64::new(0),
        };
        let mut pcm = [0.25_f32, -0.25];
        let input = cat::AudioBufList {
            number_buffers: 1,
            buffers: [cat::AudioBuf {
                number_channels: 1,
                data_bytes_size: 8,
                data: pcm.as_mut_ptr().cast(),
            }],
        };
        on_io(
            &state,
            &input,
            1,
            &cat::AudioTimeStamp::invalid(),
            &AtomicBool::new(false),
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.samples, pcm);
        assert!(seen.flags[0].contains(FrameFlags::TIMESTAMP_ERROR));
    }
    #[test]
    fn changed_channel_layout_is_not_delivered_under_the_old_format() {
        let seen = Arc::new(Mutex::new(Observed::default()));
        let state = IoState {
            sink: Mutex::new(Some(CaptureSink::new(Box::new(FixtureSink(seen.clone()))))),
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
        on_io(
            &state,
            &input,
            2,
            &cat::AudioTimeStamp::invalid(),
            &AtomicBool::new(false),
        );
        assert_eq!(state.device_frames.load(Ordering::Acquire), 2);
        input.buffers[0].number_channels = 1;
        on_io(
            &state,
            &input,
            2,
            &cat::AudioTimeStamp::invalid(),
            &AtomicBool::new(false),
        );
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
