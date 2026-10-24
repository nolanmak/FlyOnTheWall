//! The Core Audio process tap itself.
//!
//! The flow, and the reason for each step:
//!
//! 1. `CATapDescription` built with the *stereo global tap excluding
//!    processes* initializer. Global rather than per-process because browser
//!    meetings render audio from helper processes that cannot be reliably
//!    resolved to a bundle ID — a global exclude-based tap sidesteps that
//!    entirely by capturing the whole system mixdown.
//! 2. `AudioHardwareCreateProcessTap` — the TCC-gated call. The permission
//!    prompt does not fire here; it fires on `AudioDeviceStart` below.
//! 3. Read `kAudioTapPropertyUID` and `kAudioTapPropertyFormat`. **Never
//!    assume 48 kHz** — read the ASBD every time (CAP-07).
//! 4. Build a **tap-only** private aggregate device. No
//!    `kAudioAggregateDeviceMainSubDeviceKey`, no sub-device list: verified
//!    unnecessary, and omitting them removes drift compensation, the
//!    default-output lookup and default-device-change tracking from the
//!    critical path.
//! 5. `AudioDeviceCreateIOProcIDWithBlock` on a **dedicated serial dispatch
//!    queue**. Passing `None` silently fails to register the block on
//!    macOS 26 — always pass `Some`.
//! 6. `AudioDeviceStart`.
//!
//! Teardown explicitly stops and unregisters the callback, drains its queue,
//! then destroys the aggregate and process tap. Unconfirmed cleanup retains
//! callback lifetimes and blocks subsequent capture until process restart.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cidre::{
    arc, blocks, cat, cf,
    core_audio::{
        self as ca, AggregateDevice, TapDesc, aggregate_device_keys as agg, hardware::sub_tap_keys,
    },
    dispatch, ns,
};

use super::capture_assets::NativeAssets;
use super::capture_lifecycle::{CaptureOwner, Failure};
use crate::clock;
use crate::error::TapError;
use crate::format::{SampleFormat, StreamFormat};
use crate::frames::{CaptureTimestamp, FrameFlags, FrameSink};
use crate::ids::{AppRef, TapId};

/// Everything the IOProc block needs, allocated before the tap starts.
struct IoState {
    sink: Mutex<Option<Box<dyn FrameSink>>>,
    device_frames: AtomicU64,
}

/// A running system-audio tap.
pub struct SystemTap {
    id: TapId,
    excluded: Vec<AppRef>,
    hint: StreamFormat,
    running: Option<Running>,
    cleanup_failure: Option<Failure>,
    state: Arc<IoState>,
}

/// The live Core Audio objects. Dropping this tears everything down in order.
struct Running {
    format: StreamFormat,
    capture: CaptureOwner<NativeAssets>,
}

impl std::fmt::Debug for SystemTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemTap")
            .field("id", &self.id)
            .field("excluded", &self.excluded)
            .field("running", &self.running.is_some())
            .field(
                "format",
                &self.running.as_ref().map_or(self.hint, |r| r.format),
            )
            .finish()
    }
}

impl SystemTap {
    /// Prepare a tap. Nothing is created until [`crate::AudioTap::start`].
    pub fn new(id: TapId, excluded: &[AppRef]) -> Result<Self, TapError> {
        Ok(Self {
            id,
            excluded: excluded.to_vec(),
            // A hint only. Overwritten with the ASBD the platform reports.
            hint: StreamFormat::new(48_000, 2, SampleFormat::F32),
            running: None,
            cleanup_failure: None,
            state: Arc::new(IoState {
                sink: Mutex::new(None),
                device_frames: AtomicU64::new(0),
            }),
        })
    }

    fn build(&mut self, sink: Box<dyn FrameSink>) -> Result<Running, TapError> {
        // --- 1. Describe the tap -----------------------------------------
        let pids: Vec<arc::R<ns::Number>> = self
            .excluded
            .iter()
            .filter_map(|a| match a {
                AppRef::Pid(pid) => Some(ns::Number::with_i32(*pid as i32)),
                // Bundle-ID and name exclusion need CATapDescription.bundleIDs
                // (macOS 26+); open_system rejects those scopes already.
                _ => None,
            })
            .collect();
        let refs: Vec<&ns::Number> = pids.iter().map(|n| n.as_ref()).collect();
        let exclude = ns::Array::from_slice(&refs);

        let mut desc = TapDesc::with_stereo_global_tap_excluding_processes(&exclude);
        desc.set_name(Some(ns::str!(c"FlyOnTheWall system audio")));
        // Private: keeps the tap out of the user's Sound settings and Audio
        // MIDI Setup.
        //
        // It does NOT keep the aggregate device built around it out of
        // `kAudioHardwarePropertyDevices` — measured, and it matters: creating
        // and destroying this aggregate fires the system device-list
        // notification, so anything listening to that property sees our own
        // rebuild as a device change. See `crate::device_change`'s
        // `DeviceChangeKind::DeviceList`, which is why that is not a rebuild
        // trigger.
        desc.set_private(true);
        // Exclusive means `processes` is an EXCLUDE list. Inverting this
        // silently flips it to an include list and captures nothing.
        desc.set_exclusive(true);
        // Never mute the live call to record it.
        desc.set_mute_behavior(ca::TapMuteBehavior::Unmuted);

        // --- 2. Create the tap -------------------------------------------
        let tap = desc.create_process_tap().map_err(|e| {
            TapError::platform(format!("AudioHardwareCreateProcessTap failed: {e:?}"))
        })?;

        let (prepared, format) = CaptureOwner::prepare(NativeAssets::system(tap), |assets| {
            // --- 3. Read UID and the authoritative format --------------------
            let tap_uid = assets
                .system_tap()
                .uid()
                .map_err(|e| TapError::platform(format!("kAudioTapPropertyUID failed: {e:?}")))?;
            let asbd = assets.system_tap().asbd().map_err(|e| {
                TapError::platform(format!("kAudioTapPropertyFormat failed: {e:?}"))
            })?;
            let format = asbd_to_format(&asbd)?;

            // --- 4. Tap-only private aggregate device ------------------------
            let sub_tap = cf::DictionaryOf::with_keys_values(
                &[sub_tap_keys::uid(), sub_tap_keys::drift_compensation()],
                &[
                    tap_uid.as_type_ref(),
                    cf::Boolean::value_true().as_type_ref(),
                ],
            );
            let tap_list = cf::ArrayOf::from_slice(&[sub_tap.as_ref()]);
            let agg_uid = cf::Uuid::new().to_cf_string();

            let dict = cf::DictionaryOf::with_keys_values(
                &[
                    agg::uid(),
                    agg::name(),
                    agg::is_private(),
                    agg::is_stacked(),
                    agg::tap_auto_start(),
                    agg::tap_list(),
                ],
                &[
                    agg_uid.as_type_ref(),
                    cf::str!(c"FlyOnTheWall").as_type_ref(),
                    cf::Boolean::value_true().as_type_ref(),
                    cf::Boolean::value_false().as_type_ref(),
                    cf::Boolean::value_true().as_type_ref(),
                    tap_list.as_type_ref(),
                ],
            );

            let device = AggregateDevice::with_desc(&dict).map_err(|e| {
                TapError::platform(format!("AudioHardwareCreateAggregateDevice failed: {e:?}"))
            })?;

            assets.set_system_device(device);

            // --- 5. IOProc on a dedicated serial queue -----------------------
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
            let channels = format.channels as u32;

            let block = blocks::EscBlock::new5(
                move |_now: &cat::AudioTimeStamp,
                      input: &cat::AudioBufList<1>,
                      _input_time: &cat::AudioTimeStamp,
                      _output: &mut cat::AudioBufList<1>,
                      _output_time: &cat::AudioTimeStamp| {
                    on_io(&state, input, channels);
                },
            );

            assets.set_callback(block, queue);
            Ok(format)
        })?;
        let capture = prepared.start_prepared()?;
        Ok(Running { format, capture })
    }
}

/// The real-time callback.
///
/// Runs on a Core Audio real-time thread. It must not allocate, lock for long,
/// log, or do I/O. The `Mutex` here is only ever contended at start/stop, and
/// `try_lock` means the audio thread never waits on the control thread.
fn on_io(state: &IoState, input: &cat::AudioBufList<1>, channels: u32) {
    let Ok(mut guard) = state.sink.try_lock() else {
        // Start or stop is in progress. Dropping one buffer is strictly better
        // than blocking a real-time thread.
        return;
    };
    let Some(sink) = guard.as_mut() else {
        return;
    };

    let buf = &input.buffers[0];
    if buf.data.is_null() || buf.data_bytes_size == 0 {
        return;
    }
    let samples = buf.data_bytes_size as usize / std::mem::size_of::<f32>();
    // SAFETY: Core Audio guarantees `data` points to `data_bytes_size` bytes
    // of interleaved f32 for the lifetime of this callback, and we neither
    // retain nor mutate it.
    let pcm = unsafe { std::slice::from_raw_parts(buf.data as *const f32, samples) };

    let frames = samples as u64 / u64::from(channels.max(1));
    let before = state.device_frames.fetch_add(frames, Ordering::AcqRel);

    // Bit-exact silence is the signal CAP-05's watchdog keys off: on macOS 26
    // a tap can deliver all-zero buffers for minutes while the IOProc keeps
    // firing normally, which is otherwise indistinguishable from a quiet room.
    let mut flags = FrameFlags::empty();
    flags.set(FrameFlags::SILENT, pcm.iter().all(|s| *s == 0.0));

    sink.on_frames(pcm, CaptureTimestamp::new(before, clock::host_ns()), flags);
}

fn asbd_to_format(asbd: &cat::AudioStreamBasicDesc) -> Result<StreamFormat, TapError> {
    let format = StreamFormat::new(
        asbd.sample_rate as u32,
        asbd.channels_per_frame as u16,
        SampleFormat::F32,
    );
    if !format.is_plausible() {
        return Err(TapError::platform(format!(
            "tap reported an implausible format: {} Hz, {} ch — this usually \
             means an uninitialised ASBD was read",
            asbd.sample_rate, asbd.channels_per_frame
        )));
    }
    Ok(format)
}

impl Drop for SystemTap {
    fn drop(&mut self) {
        // Revoke the application sink even when callers rely on Drop alone.
        let _ = crate::tap::AudioTap::stop(self);
    }
}

impl crate::tap::AudioTap for SystemTap {
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
        NativeAssets::check_system()?;
        if self.running.is_some() {
            return Err(TapError::platform("tap is already started"));
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
