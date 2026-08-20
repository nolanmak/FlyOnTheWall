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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{SampleFormat, StreamFormat, TapId};
use fotw_web::{RecorderControl, RecorderError};
use fotwd::recording::DaemonRecorder;

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

/// A recorder whose taps are a file and whose finisher only counts.
///
/// The ceiling is short so a test that forgets to stop still ends.
fn recorder(root: &std::path::Path, finished: Arc<AtomicU64>) -> DaemonRecorder {
    DaemonRecorder::with_parts(
        root.to_path_buf(),
        tokio::runtime::Handle::current(),
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
        Box::new(move |_root, _outcome| {
            finished.fetch_add(1, Ordering::Relaxed);
        }),
        Duration::from_secs(5),
        // Generous: these taps start instantly, and a deadline that raced the
        // scheduler would make the suite flaky rather than strict.
        Duration::from_secs(10),
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
    assert!(
        stopping.is_recording(),
        "stop reports the truth: the session is still finalizing"
    );

    assert!(
        until(|| finished.load(Ordering::Relaxed) == 1).await,
        "the session never finished"
    );
    assert!(
        until(|| !rec.status().is_recording()).await,
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
    assert!(until(|| !rec.status().is_recording()).await);

    rec.start().expect("a second meeting must be startable");
    assert!(rec.status().is_recording());
    rec.stop().ok();
}
