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
//!    critical path. **No tap auto-start** either: it makes IO wait for a
//!    tapped process to *begin* playing, which a call already running on a
//!    Bluetooth HFP headset never does (see [`aggregate_description`]).
//!    The IOProc format is then read from the aggregate's input stream, not
//!    assumed from the tap's ASBD (see [`io_format`]).
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
use super::capture_clock::CaptureSink;
use super::capture_lifecycle::{CaptureOwner, Failure};
use crate::clock;
use crate::error::TapError;
use crate::format::{SampleFormat, StreamFormat};
use crate::frames::{FrameFlags, FrameSink};
use crate::ids::{AppRef, TapId};

/// Everything the IOProc block needs, allocated before the tap starts.
struct IoState {
    sink: Mutex<Option<CaptureSink>>,
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

// Builds only an owned descriptor; does not create a process tap or capture.
fn exclusion_description(excluded: &[AppRef]) -> Result<arc::R<TapDesc>, TapError> {
    let bundle_supported = cidre::api::macos_available("26.0");
    let plan = crate::system_exclusions::resolve(excluded, bundle_supported, |pid| {
        let pid = i32::try_from(pid)
            .map_err(|_| TapError::unsupported("process ID exceeds platform range"))?;
        ca::Process::with_pid(pid)
            .map(|process| process.0.0)
            .map_err(|e| TapError::platform(format!("could not resolve audio process: {e:?}")))
    })?;
    let objects: Vec<_> = plan
        .process_objects
        .iter()
        .map(|id| ns::Number::with_u32(*id))
        .collect();
    let refs: Vec<&ns::Number> = objects.iter().map(|n| n.as_ref()).collect();
    let exclude = ns::Array::from_slice(&refs);
    let mut desc = TapDesc::with_stereo_global_tap_excluding_processes(&exclude);
    if !plan.bundle_ids.is_empty() {
        let bundles: Vec<_> = plan
            .bundle_ids
            .iter()
            .map(|id| ns::String::with_str(id))
            .collect();
        let refs: Vec<&ns::String> = bundles.iter().map(|id| id.as_ref()).collect();
        let bundles = ns::Array::from_slice(&refs);
        // SAFETY: resolve refuses bundle selectors below macOS 26, and
        // the runtime check above establishes availability of both selectors.
        unsafe {
            desc.set_bundle_ids(&bundles);
            desc.set_process_restore_enabled(true);
        }
    }
    Ok(desc)
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
        let mut desc = exclusion_description(&self.excluded)?;
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
            let tap_format = asbd_to_format(&asbd)?;

            // --- 4. Tap-only private aggregate device ------------------------
            let agg_uid = cf::Uuid::new().to_cf_string();
            let dict = aggregate_description(&agg_uid, &tap_uid);

            let device = AggregateDevice::with_desc(&dict).map_err(|e| {
                TapError::platform(format!("AudioHardwareCreateAggregateDevice failed: {e:?}"))
            })?;

            let format = io_format(tap_format, device.input_asbd());
            assets.set_system_device(device);

            // --- 5. IOProc on a dedicated serial queue -----------------------
            self.state.device_frames.store(0, Ordering::Release);
            {
                let mut guard = self
                    .state
                    .sink
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                *guard = Some(CaptureSink::new(sink));
            }

            let queue = dispatch::Queue::serial_with_ar_pool();
            let state = Arc::clone(&self.state);
            let channels = format.channels as u32;

            let block = blocks::EscBlock::new5(
                move |_now: &cat::AudioTimeStamp,
                      input: &cat::AudioBufList<1>,
                      input_time: &cat::AudioTimeStamp,
                      _output: &mut cat::AudioBufList<1>,
                      _output_time: &cat::AudioTimeStamp| {
                    on_io(&state, input, channels, input_time);
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
fn on_io(
    state: &IoState,
    input: &cat::AudioBufList<1>,
    channels: u32,
    input_time: &cat::AudioTimeStamp,
) {
    let Ok(mut guard) = state.sink.try_lock() else {
        // Start or stop is in progress. Dropping one buffer is strictly better
        // than blocking a real-time thread.
        return;
    };
    let Some(sink) = guard.as_mut() else {
        return;
    };

    // Never reinterpret a buffer whose layout disagrees with the format we
    // reported: a route flip to mono read as stereo halves every frame count.
    // The typed error is a callback fault, so the supervisor rebuilds and
    // re-reads the format instead of recording garbage.
    let buf = &input.buffers[0];
    let channels_u64 = u64::from(channels);
    if input.number_buffers != 1
        || channels == 0
        || buf.number_channels != channels
        || !u64::from(buf.data_bytes_size).is_multiple_of(channels_u64 * 4)
    {
        sink.on_error(TapError::StreamFormatChanged);
        return;
    }
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

    sink.on_input(pcm, input_time, before, frames, flags);
}

/// The composition of our tap-only private aggregate device.
fn aggregate_description(
    agg_uid: &cf::String,
    tap_uid: &cf::String,
) -> arc::R<cf::DictionaryOf<cf::String, cf::Type>> {
    let sub_tap = cf::DictionaryOf::with_keys_values(
        &[sub_tap_keys::uid(), sub_tap_keys::drift_compensation()],
        &[
            tap_uid.as_type_ref(),
            cf::Boolean::value_true().as_type_ref(),
        ],
    );
    let tap_list = cf::ArrayOf::from_slice(&[sub_tap.as_ref()]);
    cf::DictionaryOf::with_keys_values(
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
            // Never auto-start. With it, AudioDeviceStart defers IO until a
            // tapped process *begins* receiving audio. Audio that was already
            // flowing when we started — a call on a Bluetooth headset in HFP —
            // never produces that edge, so the IOProc never runs (zero
            // buffers, every rebuild identical), and on macOS 26.3 coreaudiod
            // parks other processes' input starts in BlockStartOnTapStart
            // behind the unstarted tap (#268). Nothing playing at all has the
            // same no-buffer signature (#270). Chromium's CATap loopback uses
            // this same tap-only, no-auto-start composition.
            cf::Boolean::value_false().as_type_ref(),
            tap_list.as_type_ref(),
        ],
    )
}

/// The format every IOProc buffer is delivered in.
///
/// The aggregate's input stream format is what Core Audio hands the IOProc.
/// The tap's own ASBD is an estimate of the mixdown and can disagree with it:
/// on a Bluetooth headset in HFP the tap still reports 48 kHz while the route
/// runs at 16 kHz, and labelling 16 kHz buffers as 48 kHz plays the call back
/// at 3x speed. The tap's format is the fallback only when the aggregate
/// cannot describe its stream.
fn io_format<E>(
    tap: StreamFormat,
    aggregate: Result<cat::AudioStreamBasicDesc, E>,
) -> StreamFormat {
    aggregate
        .ok()
        .filter(|asbd| asbd.sample_rate.is_finite() && asbd.sample_rate.fract() == 0.0)
        .and_then(|asbd| asbd_to_format(&asbd).ok())
        .unwrap_or(tap)
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

#[cfg(test)]
mod exclusion_tests {
    use super::*;
    #[test]
    fn owned_bundle_descriptor_preserves_stable_selectors_without_opening_capture() {
        if !cidre::api::macos_available("26.0") {
            return;
        }
        let desc = exclusion_description(&[
            AppRef::BundleId("com.spotify.client".into()),
            AppRef::BundleId("com.brave.Browser.helper".into()),
        ])
        .unwrap();
        assert!(desc.processes().is_empty());
        // SAFETY: availability was checked; these getters read only our own descriptor.
        unsafe {
            assert!(desc.is_process_restore_enabled());
            let ids: Vec<_> = desc.bundle_ids().iter().map(|id| id.to_string()).collect();
            assert_eq!(ids, ["com.brave.Browser.helper", "com.spotify.client"]);
        }
    }
}

#[cfg(test)]
mod bluetooth_route_tests {
    //! A Bluetooth headset used as both mic and speaker runs in HFP: the
    //! output device is 16 kHz mono while the global tap still describes a
    //! 48 kHz stereo mixdown. None of these tests open a device or create a
    //! tap; they check only the descriptions and arithmetic we hand Core Audio.
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn flag(dict: &cf::DictionaryOf<cf::String, cf::Type>, key: &cf::String) -> Option<bool> {
        dict.get(key)
            .map(|v| v.equal(cf::Boolean::value_true().as_type_ref()))
    }

    #[test]
    fn aggregate_starts_without_waiting_for_a_tapped_process_to_begin_rendering() {
        // With tap auto-start, AudioDeviceStart defers IO until a tapped
        // process *begins* receiving audio. A call that was already playing
        // through an HFP headset never produces that edge, so the IOProc never
        // runs, and coreaudiod parks other processes' input starts behind it.
        let dict = aggregate_description(cf::str!(c"agg-fixture"), cf::str!(c"tap-fixture"));
        assert_eq!(flag(&dict, agg::tap_auto_start()), Some(false));
        assert_eq!(flag(&dict, agg::is_private()), Some(true));
        assert_eq!(flag(&dict, agg::is_stacked()), Some(false));
        assert!(dict.get(agg::tap_list()).is_some());
        // No physical sub-device: the headset is never opened as a clock.
        assert!(dict.get(agg::sub_device_list()).is_none());
        assert!(dict.get(agg::main_sub_device()).is_none());
    }

    #[test]
    fn io_format_is_what_the_aggregate_delivers_not_the_tap_estimate() {
        let tap = StreamFormat::new(48_000, 2, SampleFormat::F32);
        let hfp = cat::AudioStreamBasicDesc::common_f32(16_000.0, 2, true);
        assert_eq!(
            io_format::<()>(tap, Ok(hfp)),
            StreamFormat::new(16_000, 2, SampleFormat::F32),
            "reading 16 kHz buffers as 48 kHz plays the call back at 3x speed"
        );
    }

    #[test]
    fn io_format_falls_back_to_the_tap_when_the_aggregate_cannot_say() {
        let tap = StreamFormat::new(48_000, 2, SampleFormat::F32);
        assert_eq!(io_format(tap, Err("unreadable")), tap);
        let uninitialised = cat::AudioStreamBasicDesc::common_f32(0.0, 0, true);
        assert_eq!(io_format::<()>(tap, Ok(uninitialised)), tap);
    }

    struct Counting {
        samples: Arc<AtomicUsize>,
        errors: Arc<AtomicUsize>,
    }
    impl FrameSink for Counting {
        fn on_frames(&mut self, pcm: &[f32], _: crate::frames::CaptureTimestamp, _: FrameFlags) {
            self.samples.fetch_add(pcm.len(), Ordering::AcqRel);
        }
        fn on_error(&mut self, error: TapError) {
            assert!(matches!(error, TapError::StreamFormatChanged));
            self.errors.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn a_buffer_whose_layout_disagrees_with_the_format_is_a_fault_not_audio() {
        let samples = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(AtomicUsize::new(0));
        let state = IoState {
            sink: Mutex::new(Some(CaptureSink::new(Box::new(Counting {
                samples: samples.clone(),
                errors: errors.clone(),
            })))),
            device_frames: AtomicU64::new(0),
        };
        let mut pcm = [0.25_f32, -0.25, 0.5, -0.5];
        let buffer = |channels: u32, pcm: &mut [f32; 4]| cat::AudioBufList {
            number_buffers: 1,
            buffers: [cat::AudioBuf {
                number_channels: channels,
                data_bytes_size: 16,
                data: pcm.as_mut_ptr().cast(),
            }],
        };
        on_io(
            &state,
            &buffer(2, &mut pcm),
            2,
            &cat::AudioTimeStamp::invalid(),
        );
        assert_eq!(samples.load(Ordering::Acquire), 4);
        assert_eq!(state.device_frames.load(Ordering::Acquire), 2);

        // The route flipped to mono under a stereo format: halving the frame
        // count would silently double the speed of everything after it.
        on_io(
            &state,
            &buffer(1, &mut pcm),
            2,
            &cat::AudioTimeStamp::invalid(),
        );
        assert_eq!(samples.load(Ordering::Acquire), 4, "no samples appended");
        assert_eq!(state.device_frames.load(Ordering::Acquire), 2);
        assert_eq!(errors.load(Ordering::Acquire), 1);
    }
}
