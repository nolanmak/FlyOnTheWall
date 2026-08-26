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
use std::sync::atomic::{AtomicBool, Ordering};
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

    tokio::spawn(async move {
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
    stop.stop();
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

    // And the slot must be free, or Start is dead until the daemon restarts.
    // `is_active()`, not `!is_recording()`: finishing is not recording either,
    // and a slot stuck there refuses every Start just as thoroughly (#77).
    assert!(
        !rec.status().is_active(),
        "a failed start left the recorder holding the slot"
    );
    released.store(true, Ordering::Relaxed);
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
