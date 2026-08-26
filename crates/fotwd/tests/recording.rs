//! The daemon half of the Start button, driven without a device.
//!
//! `DaemonRecorder::with_parts` takes its taps and its finisher as arguments
//! for exactly this: the state machine — start, refuse a second start, stop,
//! clear the slot when the meeting is genuinely on disk — is the part that can
//! be wrong, and none of it needs Core Audio to be exercised.
//!
//! What is *not* covered here, deliberately: whether the audio grant belongs
//! to the bundle or to the terminal that launched it. That is a property of
//! the process, not of this type, and no unit test on a CI runner can observe
//! it. `DaemonRecorder::launched_as_app` exists so the daemon can say which it
//! got, and `serve` prints a warning when it is the wrong one.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{SampleFormat, StreamFormat, TapId};
use fotw_web::{RecorderControl, RecorderError, RecordingState};
use fotwd::recording::{DaemonRecorder, Finisher};

/// A data root with a `sessions/` inside it.
///
/// The nesting is load-bearing: `AuditLog::at` writes to the *parent* of the
/// sessions directory, so a flat temp dir would append every test's audit
/// entries to a shared `/tmp/audit.jsonl`.
fn tmpdir(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("fotwd-rec-{name}-{}", std::process::id()));
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

/// A recorder whose taps are a file and whose finisher is the caller's.
///
/// The ceiling is short so a test that forgets to stop still ends.
fn recorder_with(root: &Path, finish: Finisher) -> DaemonRecorder {
    DaemonRecorder::with_parts(
        root.to_path_buf(),
        tokio::runtime::Handle::current(),
        fotwd::session::SegmentTap::default(),
        Box::new(|| {
            Ok((
                Box::new(FileAudioSource::from_wav(
                    TapId::system_default(),
                    tone(30.0),
                    ReplaySpeed::Realtime,
                )),
                None,
            ))
        }),
        // Never the real keychain: see `TranscriptionFactory`. A test that
        // read it would raise an approval dialog on every rebuild.
        Box::new(|| fotwd::session::Transcription::Disabled),
        finish,
        Duration::from_secs(5),
        // Generous: these taps start instantly, and a deadline that raced the
        // scheduler would make the suite flaky rather than strict.
        Duration::from_secs(10),
    )
}

/// A recorder whose finisher only counts.
fn recorder(root: &Path, finished: Arc<AtomicU64>) -> DaemonRecorder {
    recorder_with(
        root,
        Box::new(move |_root, _outcome| {
            finished.fetch_add(1, Ordering::Relaxed);
            None
        }),
    )
}

/// A finisher the test holds open, so `Finishing` lasts long enough to observe.
///
/// The rig persists a tone in milliseconds, so a test that tried to catch the
/// finishing window by timing would race the session task and pass or fail
/// with the scheduler. This parks *inside* the finisher instead — the daemon
/// runs it on `spawn_blocking`, so blocking that thread costs the runtime
/// nothing — and the window is then exactly as wide as the assertions need.
#[derive(Default)]
struct Gate {
    inner: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    /// How many finishers have arrived at the gate.
    arrivals: u64,
    open: bool,
}

impl Gate {
    /// Called from the finisher: announce arrival, then wait to be let go.
    fn hold(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.arrivals += 1;
        self.changed.notify_all();
        while !inner.open {
            inner = self.changed.wait(inner).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Whether the session task has reached the gate — from here the recorder
    /// is finishing and stays there until [`Gate::open`].
    fn reached(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.arrivals > 0
    }

    /// Let the meeting land, so the slot clears.
    fn open(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.open = true;
        self.changed.notify_all();
    }
}

/// A recorder that cannot finish until the returned gate is opened.
fn gated_recorder(root: &Path, gate: Arc<Gate>) -> DaemonRecorder {
    recorder_with(
        root,
        Box::new(move |_root, _outcome| {
            gate.hold();
            None
        }),
    )
}

/// Wait for a predicate, so the test does not race the session task.
async fn until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_recorder_is_idle() {
    let root = tmpdir("idle");
    let rec = recorder(&root.join("sessions"), Arc::new(AtomicU64::new(0)));
    assert!(!rec.status().is_recording());
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_reports_recording_and_writes_the_audit_entry() {
    let root = tmpdir("start");
    let rec = recorder(&root.join("sessions"), Arc::new(AtomicU64::new(0)));

    let status = rec.start().expect("start");
    assert!(status.is_recording());
    assert!(status.started_at_ms.is_some());
    assert!(rec.status().is_recording());

    // CON-01: the audit entry is written before the tap opens, and it names
    // the origin so "who started this" survives the session.
    let log = std::fs::read_to_string(root.join("audit.jsonl")).expect("audit log");
    assert!(
        log.contains("\"origin\":\"web-ui\""),
        "the audit entry must name the web UI as the origin: {log}"
    );
    assert!(log.contains("session_start"));

    rec.stop().ok();
}

/// A double-clicked button must not open a second tap on the same device.
#[tokio::test(flavor = "multi_thread")]
async fn starting_twice_is_refused() {
    let root = tmpdir("twice");
    let rec = recorder(&root.join("sessions"), Arc::new(AtomicU64::new(0)));

    rec.start().expect("first start");
    let again = rec.start();

    assert!(
        matches!(again, Err(RecorderError::AlreadyRecording)),
        "the second start was not refused"
    );

    let log = std::fs::read_to_string(root.join("audit.jsonl")).unwrap();
    assert_eq!(
        log.matches("session_start").count(),
        1,
        "the refused start still wrote an audit entry"
    );

    rec.stop().ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_when_idle_says_so() {
    let root = tmpdir("stop-idle");
    let rec = recorder(&root.join("sessions"), Arc::new(AtomicU64::new(0)));
    assert!(matches!(rec.stop(), Err(RecorderError::NotRecording)));
}

/// The slot stays occupied until the meeting is genuinely on disk, so a Start
/// arriving during finalization is refused rather than opening a second tap.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_session_finishes_and_frees_the_slot() {
    let root = tmpdir("finish");
    let finished = Arc::new(AtomicU64::new(0));
    let rec = recorder(&root.join("sessions"), Arc::clone(&finished));

    rec.start().expect("start");
    tokio::time::sleep(Duration::from_millis(400)).await;

    let stopping = rec.stop().expect("stop");
    assert_eq!(
        stopping.state,
        RecordingState::Finishing,
        "stop reports the truth: capture is over, the session is still being written"
    );
    assert!(
        !stopping.is_recording(),
        "the clock the dashboard renders stops with capture, not with the file"
    );

    assert!(
        until(|| finished.load(Ordering::Relaxed) == 1).await,
        "the session never finished"
    );
    // `Idle`, not `!is_recording()`: finishing is already not recording, so
    // that spelling would have passed the instant stop tripped and told us
    // nothing about the slot.
    assert!(
        until(|| rec.status().state == RecordingState::Idle).await,
        "the slot was never cleared, so Start stays refused forever"
    );
}

/// After a full cycle the recorder is reusable — a second meeting in the same
/// daemon is the normal case, not an edge one.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_meeting_can_start_after_the_first_finishes() {
    let root = tmpdir("second");
    let finished = Arc::new(AtomicU64::new(0));
    let rec = recorder(&root.join("sessions"), Arc::clone(&finished));

    rec.start().expect("first");
    tokio::time::sleep(Duration::from_millis(300)).await;
    rec.stop().ok();
    // Waiting on `Idle` rather than `!is_recording()`: the latter is true the
    // moment stop trips, and the start below would then race the first
    // session's own teardown for the slot.
    assert!(until(|| rec.status().state == RecordingState::Idle).await);

    rec.start().expect("a second meeting must be startable");
    assert!(rec.status().is_recording());
    rec.stop().ok();
}

// --------------------------------------------------------- #77, finishing

/// The clock the dashboard draws stops with capture. It used to keep climbing
/// for as long as finalization took — measured at 23 seconds on a real
/// session, and unbounded once enrichment was in the path.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_freezes_the_clock_until_the_meeting_lands() {
    let root = tmpdir("frozen-clock");
    let gate = Arc::new(Gate::default());
    let rec = gated_recorder(&root.join("sessions"), Arc::clone(&gate));

    rec.start().expect("start");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopping = rec.stop().expect("stop");

    assert_eq!(stopping.state, RecordingState::Finishing);
    let frozen = stopping
        .elapsed_ms
        .expect("a finished meeting has a length");
    let ended = stopping.ended_at_ms.expect("and an end time");

    assert!(
        until(|| gate.reached()).await,
        "the session never reached the finisher"
    );
    let first = rec.status();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let second = rec.status();

    for status in [&first, &second] {
        assert_eq!(status.state, RecordingState::Finishing);
        assert_eq!(
            status.elapsed_ms,
            Some(frozen),
            "the clock moved after capture stopped"
        );
        assert_eq!(status.ended_at_ms, Some(ended));
    }

    gate.open();
    assert!(until(|| rec.status().state == RecordingState::Idle).await);
}

/// The guard the module header argues for, restated for the new word: a Start
/// during finalization is still refused, because `Finishing` is not `Idle`.
#[tokio::test(flavor = "multi_thread")]
async fn starting_during_finalization_is_refused_while_status_reads_finishing() {
    let root = tmpdir("start-while-finishing");
    let gate = Arc::new(Gate::default());
    let rec = gated_recorder(&root.join("sessions"), Arc::clone(&gate));

    rec.start().expect("start");
    tokio::time::sleep(Duration::from_millis(300)).await;
    rec.stop().expect("stop");
    assert!(until(|| gate.reached()).await);

    assert!(
        matches!(rec.start(), Err(RecorderError::AlreadyRecording)),
        "a second tap was opened while the first meeting was still being written"
    );
    assert_eq!(rec.status().state, RecordingState::Finishing);

    // CON-01: the refused start must not have written a second audit entry.
    let log = std::fs::read_to_string(root.join("audit.jsonl")).unwrap();
    assert_eq!(log.matches("session_start").count(), 1);

    gate.open();
    assert!(until(|| rec.status().state == RecordingState::Idle).await);
}

/// A reloaded tab presses Stop again. The frozen clock must not move — a
/// meeting that grows after it ended is a meeting nobody can trust.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_stop_keeps_the_first_end_time() {
    let root = tmpdir("second-stop");
    let gate = Arc::new(Gate::default());
    let rec = gated_recorder(&root.join("sessions"), Arc::clone(&gate));

    rec.start().expect("start");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let first = rec.stop().expect("stop");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let second = rec.stop().expect("a second stop is not an error");

    assert_eq!(second.state, RecordingState::Finishing);
    assert_eq!(second.ended_at_ms, first.ended_at_ms);
    assert_eq!(second.elapsed_ms, first.elapsed_ms);

    gate.open();
    assert!(until(|| rec.status().state == RecordingState::Idle).await);
}

/// The bridge between the two doubles. `FakeRecorder` in the web suite pins
/// these same three words from the other side; before #77 the two encoded
/// opposite contracts — the fake returned `idle` from stop, the daemon
/// returned `recording` — and nothing compared them, which is why the bug
/// survived a green CI for the life of the feature.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_cycle_spells_recording_then_finishing_then_idle() {
    let root = tmpdir("wire-cycle");
    let gate = Arc::new(Gate::default());
    let rec = gated_recorder(&root.join("sessions"), Arc::clone(&gate));

    let word = |s: &fotw_web::RecordingStatus| serde_json::to_value(s).unwrap()["state"].clone();

    let started = rec.start().expect("start");
    assert_eq!(word(&started), "recording");
    assert_eq!(word(&rec.status()), "recording");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopping = rec.stop().expect("stop");
    assert_eq!(word(&stopping), "finishing");
    assert!(until(|| gate.reached()).await);
    assert_eq!(word(&rec.status()), "finishing");

    gate.open();
    assert!(until(|| rec.status().state == RecordingState::Idle).await);
    let idle = serde_json::to_value(rec.status()).unwrap();
    assert_eq!(idle["state"], "idle");
    assert!(idle["started_at_ms"].is_null());
    assert!(idle["ended_at_ms"].is_null());
}
