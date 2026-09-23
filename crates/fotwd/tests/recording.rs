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
        // No hub behind a unit test, and the finisher here is the caller's
        // rather than `persist_and_promote`, so there is nothing to announce.
        fotwd::recording::ReadyTap::default(),
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
async fn an_unattended_recording_finalizes_on_its_own() {
    let root = tmpdir("automatic-stop");
    let gate = Arc::new(Gate::default());
    let rec = gated_recorder(&root.join("sessions"), Arc::clone(&gate));
    let started = rec.start().expect("start");
    assert_eq!(
        started.auto_stop_at_ms,
        started.started_at_ms.map(|t| t + 5000)
    );

    // Nobody presses Stop: the ceiling ends capture, the session winds down,
    // and the finisher parks. Under Option A that parking happens only after
    // the slot is freed, so reaching it proves the deadline finalized the
    // meeting *and* freed the recorder without any help from the UI.
    assert!(
        until(|| gate.reached()).await,
        "the deadline must finalize without pressing Stop"
    );
    assert_eq!(
        rec.status().state,
        RecordingState::Idle,
        "the slot frees when capture is safe, before background persist"
    );

    gate.open();
    assert!(until(|| rec.status().state == RecordingState::Idle).await);
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

/// Stop trips the finishing signal, the slot frees when capture is safe, and
/// the finisher still runs to completion in the background — the counter is the
/// proof that persist happened after the slot was already let go.
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
///
/// `stop()` is the deterministic finishing signal: it returns the frozen length
/// and end time the instant capture ends, never a climbing clock. Under Option
/// A the slot then frees before persist, so the meeting "lands" in the
/// background; the numbers `stop()` reported are the meeting's final ones and
/// nothing after capture can grow them (the second-stop test pins that).
#[tokio::test(flavor = "multi_thread")]
async fn stopping_freezes_the_clock_it_reports() {
    let root = tmpdir("frozen-clock");
    let gate = Arc::new(Gate::default());
    let rec = gated_recorder(&root.join("sessions"), Arc::clone(&gate));

    rec.start().expect("start");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopping = rec.stop().expect("stop");

    assert_eq!(stopping.state, RecordingState::Finishing);
    assert!(
        stopping.elapsed_ms.is_some(),
        "a finished meeting has a length"
    );
    assert!(stopping.ended_at_ms.is_some(), "and an end time");
    assert!(
        !stopping.is_recording(),
        "the clock stops with capture, not with the file"
    );

    // The finisher parks only after the slot frees, so reaching it proves the
    // meeting is finalizing in the background with the recorder already idle.
    assert!(
        until(|| gate.reached()).await,
        "the session never reached the finisher"
    );
    assert_eq!(
        rec.status().state,
        RecordingState::Idle,
        "the slot must free before persist, not after"
    );

    gate.open();
    assert!(until(|| rec.status().state == RecordingState::Idle).await);
}

/// Option A, the contract this change turns on: the slot frees the moment
/// capture is safe on disk — before persist — so a Start arriving while the
/// previous meeting is still finalizing in the background is *accepted*, not
/// refused. The gate parks inside the finisher, which under Option A runs only
/// after the slot is freed, so `gate.reached()` proves the slot is already open.
#[tokio::test(flavor = "multi_thread")]
async fn starting_while_the_previous_meeting_finalizes_is_allowed() {
    let root = tmpdir("start-while-finalizing");
    let gate = Arc::new(Gate::default());
    let rec = gated_recorder(&root.join("sessions"), Arc::clone(&gate));

    rec.start().expect("start");
    tokio::time::sleep(Duration::from_millis(300)).await;
    rec.stop().expect("stop");

    // The finisher is parked. Under Option A that only happens after the slot is
    // freed: persist runs in the background, not while holding the recorder.
    assert!(
        until(|| gate.reached()).await,
        "the session never reached the finisher"
    );
    assert_eq!(
        rec.status().state,
        RecordingState::Idle,
        "the slot must free before persist, not after"
    );

    // The point of the change: a second meeting starts while the first one is
    // still being written to disk.
    let second = rec.start();
    assert!(
        second.is_ok(),
        "a Start during background finalization must be accepted: {second:?}"
    );
    assert!(rec.status().is_recording());

    // CON-01: the accepted Start wrote its own audit entry — two starts, two rows.
    let log = std::fs::read_to_string(root.join("audit.jsonl")).unwrap();
    assert_eq!(log.matches("session_start").count(), 2);

    rec.stop().ok();
    gate.open();
    assert!(until(|| rec.status().state == RecordingState::Idle).await);
}

/// A reloaded tab presses Stop again. The end time is set once and never
/// moves — a meeting that grows after it ended is one nobody can trust. Under
/// Option A the slot frees as soon as capture is safe, so a later Stop may
/// instead find the meeting already gone; either way it never reports a *new*
/// end time.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_stop_never_moves_the_end_time() {
    let root = tmpdir("second-stop");
    let gate = Arc::new(Gate::default());
    let rec = gated_recorder(&root.join("sessions"), Arc::clone(&gate));

    rec.start().expect("start");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let first = rec.stop().expect("stop");
    // No await between the two Stops: the second lands in the same finishing
    // window, while the slot is still occupied, so the idempotent end time is
    // observable rather than raced away by the background finalize.
    match rec.stop() {
        Ok(second) => {
            assert_eq!(second.state, RecordingState::Finishing);
            assert_eq!(second.ended_at_ms, first.ended_at_ms);
            assert_eq!(second.elapsed_ms, first.elapsed_ms);
        }
        // The background finalize already freed the slot — also correct: the
        // meeting ended at `first`'s time and cannot grow.
        Err(RecorderError::NotRecording) => {}
        other => panic!("a second stop reported something new: {other:?}"),
    }

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
    // `stop()` is the finishing signal — the wire word the dashboard renders
    // the instant capture ends, however long the background finalize runs.
    let stopping = rec.stop().expect("stop");
    assert_eq!(word(&stopping), "finishing");

    // Under Option A the slot frees before persist, so once the finisher parks
    // the recorder already reads idle: persist is running in the background.
    assert!(until(|| gate.reached()).await);
    assert_eq!(word(&rec.status()), "idle");

    gate.open();
    assert!(until(|| rec.status().state == RecordingState::Idle).await);
    let idle = serde_json::to_value(rec.status()).unwrap();
    assert_eq!(idle["state"], "idle");
    assert!(idle["started_at_ms"].is_null());
    assert!(idle["ended_at_ms"].is_null());
}
