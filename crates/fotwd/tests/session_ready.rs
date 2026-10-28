//! Reporting "recording" only once capture is genuinely live.
//!
//! # The failure this exists to prevent
//!
//! `DaemonRecorder::start()` used to record its state and spawn the session,
//! then answer `recording` immediately. But the taps are started *inside* that
//! task, and a Core Audio device can block in `start()` forever rather than
//! failing — which is exactly what a stale HAL client does on macOS. The UI
//! then showed a red RECORDING badge, ticking elapsed time, and an empty disk.
//!
//! That is the precise failure this whole project is written against: a
//! machine that reports success where the user gets silence. So `start()` now
//! waits for the session to say capture is live, and answers an error naming
//! the fix if it does not.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, FrameSink, SampleFormat, StreamFormat, TapError, TapId};
use fotw_web::{RecorderControl, RecorderError, RecordingState};
use fotwd::recording::DaemonRecorder;
use fotwd::session::{self, ReadySignal, SessionControl, Transcription};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("fotwd-rdy-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("sessions")).unwrap();
    base
}

fn tone(seconds: f32) -> WavData {
    let format = StreamFormat::new(48_000, 2, SampleFormat::I16);
    let n = (48_000.0 * seconds) as usize;
    let mut samples = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / 48_000.0;
        let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        samples.push(v);
        samples.push(v);
    }
    WavData { format, samples }
}

fn file_tap() -> Box<dyn AudioTap> {
    Box::new(FileAudioSource::from_wav(
        TapId::system_default(),
        tone(30.0),
        ReplaySpeed::Realtime,
    ))
}

/// A tap that blocks in `start()` until the test releases it.
///
/// Not a contrived case: `MicTap::build` does exactly this against a Core
/// Audio HAL that still believes a dead client holds the device, and no
/// timeout inside the process can cancel the syscall it is stuck in.
///
/// The release flag exists only so the *test binary* can exit. A tap that
/// blocked forever would hold a runtime worker that cannot be aborted — the
/// suite would pass and then hang, which is a worse failure than the bug.
struct WedgedTap {
    id: TapId,
    released: Arc<AtomicBool>,
}

impl WedgedTap {
    fn wedged() -> (Box<dyn AudioTap>, Arc<AtomicBool>) {
        let released = Arc::new(AtomicBool::new(false));
        (
            Box::new(Self {
                id: TapId::system_default(),
                released: Arc::clone(&released),
            }),
            released,
        )
    }
}

impl AudioTap for WedgedTap {
    fn id(&self) -> &TapId {
        &self.id
    }
    fn format(&self) -> StreamFormat {
        StreamFormat::new(48_000, 2, SampleFormat::I16)
    }
    fn format_is_authoritative(&self) -> bool {
        false
    }
    fn start(&mut self, _sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        while !self.released.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(TapError::Unsupported("released".into()))
    }
    fn stop(&mut self) -> Result<(), TapError> {
        Ok(())
    }
}

// ------------------------------------------------------------ the signal

#[test]
fn a_fresh_ready_signal_is_not_ready() {
    let r = ReadySignal::new();
    assert!(!r.is_ready());
    r.signal();
    assert!(r.is_ready());
}

#[test]
fn waiting_on_an_already_ready_signal_returns_at_once() {
    let r = ReadySignal::new();
    r.signal();
    let began = Instant::now();
    assert!(r.wait_timeout(Duration::from_secs(30)));
    assert!(began.elapsed() < Duration::from_secs(1));
}

#[test]
fn waiting_on_a_signal_nobody_trips_times_out() {
    let r = ReadySignal::new();
    let began = Instant::now();
    assert!(!r.wait_timeout(Duration::from_millis(300)));
    assert!(began.elapsed() >= Duration::from_millis(250));
}

/// The wake must cross threads: the session trips it on the runtime while the
/// recorder waits on a blocking-pool thread.
#[test]
fn a_signal_tripped_from_another_thread_wakes_the_waiter() {
    let r = ReadySignal::new();
    let held = r.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        held.signal();
    });
    assert!(r.wait_timeout(Duration::from_secs(5)));
}

// --------------------------------------------------------------- the session

#[tokio::test(flavor = "multi_thread")]
async fn a_session_reports_ready_once_the_taps_are_running() {
    let root = tmpdir("ready");
    let control = SessionControl::new();
    let ready = control.ready.clone();
    let stop = control.stop.clone();

    let task = tokio::spawn(async move {
        session::run_with_control(
            &root.join("sessions"),
            file_tap(),
            None,
            Transcription::Disabled,
            Duration::from_secs(30),
            control,
        )
        .await
    });

    assert!(
        ready.wait_timeout(Duration::from_secs(10)),
        "the session never reported that capture was live"
    );
    let started = ready
        .capture_started_at_ms()
        .expect("WAL start published with readiness");
    assert_eq!(ready.capture_ended_at_ms(), None);
    stop.stop();
    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.started_at_ms, started);
    let ended = ready
        .capture_ended_at_ms()
        .expect("stop published before completion");
    assert!(ended >= started);
    assert_eq!(outcome.capture_ended_at_ms, Some(ended));
    ready.signal_capture_stopped(ended.saturating_add(60_000));
    assert_eq!(ready.capture_ended_at_ms(), Some(ended));
}

/// A session whose tap wedges must never claim capture is live.
#[tokio::test(flavor = "multi_thread")]
async fn a_wedged_tap_never_reports_ready() {
    let root = tmpdir("wedged-session");
    let control = SessionControl::new();
    let ready = control.ready.clone();
    let (tap, released) = WedgedTap::wedged();

    tokio::spawn(async move {
        session::run_with_control(
            &root.join("sessions"),
            tap,
            None,
            Transcription::Disabled,
            Duration::from_secs(30),
            control,
        )
        .await
    });

    assert!(
        !ready.wait_timeout(Duration::from_millis(600)),
        "a tap that never started was reported as live"
    );
    released.store(true, Ordering::Relaxed);
}

// -------------------------------------------------------------- the recorder

fn recorder(
    root: &std::path::Path,
    taps: fotwd::recording::TapOpener,
    deadline: Duration,
) -> DaemonRecorder {
    DaemonRecorder::with_parts(
        root.to_path_buf(),
        tokio::runtime::Handle::current(),
        fotwd::session::SegmentTap::default(),
        // These tests are about the start deadline; nothing here persists a
        // meeting, so there is nothing to announce.
        fotwd::recording::ReadyTap::default(),
        taps,
        Box::new(|| Transcription::Disabled),
        Box::new(|_root, _outcome| None),
        Duration::from_secs(5),
        deadline,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_healthy_device_reports_recording() {
    let root = tmpdir("healthy");
    let rec = recorder(
        &root.join("sessions"),
        Box::new(|| Ok((file_tap(), None))),
        Duration::from_secs(10),
    );

    let status = rec.start().expect("start");
    assert!(status.is_recording());
    rec.stop().ok();
}

/// The whole point. A wedged device must produce an error the user can act on,
/// not a red badge over an empty disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_wedged_device_fails_the_start_instead_of_lying() {
    let root = tmpdir("wedged");
    let released = Arc::new(AtomicBool::new(false));
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let _release_on_unwind = Release(released.clone());
    let for_opener = Arc::clone(&released);
    let rec = recorder(
        &root.join("sessions"),
        Box::new(move || {
            Ok((
                Box::new(WedgedTap {
                    id: TapId::system_default(),
                    released: Arc::clone(&for_opener),
                }) as Box<dyn AudioTap>,
                None,
            ))
        }),
        Duration::from_millis(700),
    );

    let began = Instant::now();
    let err = rec
        .start()
        .expect_err("a wedged device must not report success");

    assert!(
        began.elapsed() < Duration::from_secs(10),
        "start blocked far past its readiness deadline"
    );
    let RecorderError::Failed(message) = err else {
        panic!("expected a Failed, got something else");
    };
    assert!(
        message.contains("coreaudiod"),
        "the error must name the fix, since nothing in-process can clear a \
         wedged HAL: {message}"
    );

    // A timeout bounds the caller, not the native operation. Retain the owner
    // until it exits so another Start cannot open a second concurrent device.
    assert_eq!(rec.status().state, RecordingState::Starting);
    assert!(rec.status().preparation_cancel_requested);
    assert!(matches!(rec.start(), Err(RecorderError::AlreadyRecording)));
    released.store(true, Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(5), async {
        while rec.status().state != RecordingState::Idle {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("released native setup did not retire its owner");
}

/// CON-01 puts the audit entry *before* the tap opens on purpose, so a crash
/// during capture still leaves the record of who asked. A start that then
/// fails therefore does leave an entry — and must still leave the recorder
/// idle, or Start is dead until the daemon restarts.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_that_cannot_open_a_device_leaves_the_recorder_idle() {
    let root = tmpdir("no-device");
    let rec = recorder(
        &root.join("sessions"),
        Box::new(|| Err("no such device".to_owned())),
        Duration::from_millis(500),
    );

    let err = rec
        .start()
        .expect_err("a missing device must not report success");
    assert!(matches!(err, RecorderError::Failed(_)));
    assert_eq!(rec.status().state, RecordingState::Idle);

    let log = std::fs::read_to_string(root.join("audit.jsonl")).expect("audit log");
    assert!(
        log.contains("session_start"),
        "CON-01 wants the record of who asked, even for a start that failed"
    );

    // And the recorder is still usable afterwards.
    let healthy = recorder(
        &root.join("sessions"),
        Box::new(|| Ok((file_tap(), None))),
        Duration::from_secs(10),
    );
    assert!(healthy.start().is_ok());
    healthy.stop().ok();
}

#[test]
fn capture_clock_is_published_with_readiness_and_stop_is_frozen_before_drain() {
    let ready = ReadySignal::new();
    assert_eq!(ready.capture_started_at_ms(), None);
    assert_eq!(ready.capture_ended_at_ms(), None);
    ready.signal_capture("fixture-session".into(), 1_000);
    assert!(ready.is_ready());
    assert_eq!(ready.capture_started_at_ms(), Some(1_000));
    ready.signal_capture_stopped(1_250);
    ready.signal_capture_stopped(9_000); // late cleanup cannot lengthen capture
    assert_eq!(ready.capture_ended_at_ms(), Some(1_250));
    assert_eq!(ready.session_id().as_deref(), Some("fixture-session"));
}

#[test]
fn a_stop_during_startup_or_clock_rollback_never_precedes_capture_start() {
    let ready = ReadySignal::new();
    ready.signal_capture_stopped(900);
    ready.signal_capture("fixture-session".into(), 1_000);
    assert_eq!(ready.capture_ended_at_ms(), Some(1_000));
    ready.signal_capture_stopped(2_000);
    assert_eq!(ready.capture_ended_at_ms(), Some(1_000));
}

/// Reproduced on macOS 26.3: system-first blocks AudioDeviceStart for the mic.
/// Model that failure as an error so the regression terminates on every OS.
struct OrderedTap {
    inner: Box<dyn AudioTap>,
    system_started: Arc<AtomicBool>,
    is_mic: bool,
    /// Mic starts refused because system capture was already running.
    refused: Arc<AtomicUsize>,
}
impl AudioTap for OrderedTap {
    fn id(&self) -> &TapId {
        self.inner.id()
    }
    fn format_is_authoritative(&self) -> bool {
        self.inner.format_is_authoritative()
    }
    fn format(&self) -> StreamFormat {
        self.inner.format()
    }
    fn start(&mut self, sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        if self.is_mic && self.system_started.load(Ordering::SeqCst) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(TapError::platform(
                "microphone cannot start after system capture",
            ));
        }
        if !self.is_mic {
            self.system_started.store(true, Ordering::SeqCst);
        }
        self.inner.start(sink)
    }
    fn stop(&mut self) -> Result<(), TapError> {
        self.inner.stop()
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn microphone_starts_before_system_so_both_tracks_capture() {
    let root = tmpdir("mic-first");
    let flag = Arc::new(AtomicBool::new(false));
    let system = Box::new(OrderedTap {
        inner: file_tap(),
        system_started: flag.clone(),
        is_mic: false,
        refused: Arc::default(),
    });
    let mic = Box::new(OrderedTap {
        inner: file_tap(),
        system_started: flag,
        is_mic: true,
        refused: Arc::default(),
    });
    let outcome = session::run_with_control(
        &root.join("sessions"),
        system,
        Some(mic),
        Transcription::Disabled,
        Duration::from_millis(150),
        SessionControl::new(),
    )
    .await
    .unwrap();
    assert!(
        std::fs::metadata(outcome.dir.join("mic.pcm"))
            .unwrap()
            .len()
            > 0,
        "startup order must preserve the microphone track"
    );
    assert!(
        std::fs::metadata(outcome.dir.join("system.pcm"))
            .unwrap()
            .len()
            > 0
    );
}

struct OrderedPlatform {
    inner: fotw_audio::platform::file::FilePlatform,
    system_started: Arc<AtomicBool>,
    refused: Arc<AtomicUsize>,
}
impl fotw_audio::AudioPlatform for OrderedPlatform {
    fn caps(&self) -> fotw_audio::PlatformCaps {
        self.inner.caps()
    }
    fn permission(&self, p: fotw_audio::Permission) -> fotw_audio::PermissionState {
        self.inner.permission(p)
    }
    fn request_permission(
        &self,
        p: fotw_audio::Permission,
    ) -> fotw_audio::BoxFuture<'static, fotw_audio::PermissionState> {
        self.inner.request_permission(p)
    }
    fn mics(&self) -> Vec<fotw_audio::DeviceInfo> {
        self.inner.mics()
    }
    fn capturable_apps(&self) -> Vec<fotw_audio::AppInfo> {
        self.inner.capturable_apps()
    }
    fn events(&self) -> std::sync::mpsc::Receiver<fotw_audio::PlatformEvent> {
        self.inner.events()
    }
    fn open_mic(
        &self,
        d: &fotw_audio::DeviceId,
        h: fotw_audio::FormatRequest,
    ) -> Result<Box<dyn AudioTap>, TapError> {
        Ok(Box::new(OrderedTap {
            inner: self.inner.open_mic(d, h)?,
            system_started: self.system_started.clone(),
            is_mic: true,
            refused: self.refused.clone(),
        }))
    }
    fn open_system(
        &self,
        s: fotw_audio::SystemScope,
        h: fotw_audio::FormatRequest,
    ) -> Result<Box<dyn AudioTap>, TapError> {
        Ok(Box::new(OrderedTap {
            inner: self.inner.open_system(s, h)?,
            system_started: self.system_started.clone(),
            is_mic: false,
            refused: self.refused.clone(),
        }))
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn supervised_start_preserves_both_tracks_with_order_sensitive_devices() {
    let root = tmpdir("supervised-mic-first");
    let platform = Arc::new(OrderedPlatform {
        inner: fotw_audio::platform::file::FilePlatform::new(ReplaySpeed::Realtime)
            .with_mic(tone(2.0))
            .with_system(tone(2.0)),
        system_started: Arc::new(AtomicBool::new(false)),
        refused: Arc::default(),
    });
    let refused = platform.refused.clone();
    let outcome = session::run_supervised(
        &root.join("sessions"),
        platform,
        Arc::new(fotw_audio::clock::HostClock),
        Transcription::Disabled,
        Duration::from_millis(150),
        SessionControl::new(),
    )
    .await
    .unwrap();
    for track in ["mic.pcm", "system.pcm"] {
        assert!(
            std::fs::metadata(outcome.dir.join(track)).unwrap().len() > 0,
            "missing {track}"
        );
    }
    // Recovery can mask a refused first start; on macOS 26.3 that first
    // start never returns, so the mic must never be started after the tap.
    assert_eq!(
        refused.load(Ordering::SeqCst),
        0,
        "the microphone was started after system capture"
    );
}
