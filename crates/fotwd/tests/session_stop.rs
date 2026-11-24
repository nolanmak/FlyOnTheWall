//! Ending a session on demand rather than on a stopwatch.
//!
//! The CLI records for a fixed number of seconds, which is the right shape for
//! `fotwd record 3600`. A Start/Stop button is the other shape: nobody knows
//! how long the meeting is when it begins. Both must end the same way — taps
//! stopped, WAL finalized, transcript drained — so `run_with_stop` is the real
//! function and `run` is the delegate that passes a signal nobody holds.

#[path = "fixtures/scratch.rs"]
mod scratch;
#[path = "fixtures/tone.rs"]
mod tone;

use scratch::scratch;
use tone::tone;

use std::time::{Duration, Instant};

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, TapId};
use fotwd::session::{self, StopSignal, Transcription};

fn source(data: WavData) -> Box<dyn AudioTap> {
    Box::new(FileAudioSource::from_wav(
        TapId::system_default(),
        data,
        ReplaySpeed::Realtime,
    ))
}

#[test]
fn a_fresh_signal_is_not_stopped() {
    let s = StopSignal::new();
    assert!(!s.is_stopped());
    s.stop();
    assert!(s.is_stopped());
}

/// Clones share one latch — the web handler holds one end while the session
/// task holds the other.
#[test]
fn a_clone_shares_the_latch() {
    let s = StopSignal::new();
    let held = s.clone();
    held.stop();
    assert!(s.is_stopped(), "stopping a clone must stop the original");
}

/// The point of the whole change: a session opened with an hour on the clock
/// ends when it is told to, not an hour later.
#[tokio::test]
async fn a_stop_ends_a_long_session_early() {
    let root_dir = scratch("stop-early");
    let root = root_dir.path().join("sessions");
    let stop = StopSignal::new();

    let held = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        held.stop();
    });

    let began = Instant::now();
    let outcome = session::run_with_stop(
        &root,
        source(tone(30.0)),
        None,
        Transcription::Disabled,
        Duration::from_secs(3_600),
        stop,
    )
    .await
    .unwrap();
    let took = began.elapsed();

    assert!(
        root_dir.path().join("model-work.lock").is_file(),
        "capture admission must use this test's owned data root"
    );

    assert!(
        took < Duration::from_secs(10),
        "stop was ignored; the session ran for {took:?}"
    );
    assert!(
        outcome.system_buffers.total > 0,
        "the tap delivered nothing"
    );
    assert!(
        outcome.captured_audio(),
        "audio captured before the stop must still be real"
    );
}

/// Whichever comes first. An unbounded session is a real failure mode — it
/// runs until the disk fills, and `retention::recording_in_flight` vetoes the
/// sweeper for as long as it lives — so the duration stays a ceiling.
#[tokio::test]
async fn the_duration_still_ends_a_session_nobody_stops() {
    let root_dir = scratch("stop-duration");
    let root = root_dir.path().join("sessions");
    let began = Instant::now();

    let outcome = session::run_with_stop(
        &root,
        source(tone(30.0)),
        None,
        Transcription::Disabled,
        Duration::from_millis(900),
        StopSignal::new(),
    )
    .await
    .unwrap();

    assert!(
        began.elapsed() < Duration::from_secs(10),
        "the duration ceiling was not honoured"
    );
    assert!(outcome.system_buffers.total > 0);
}

/// A signal already tripped before the session opens must not hang waiting for
/// a second `stop()` that will never come.
#[tokio::test]
async fn a_signal_stopped_in_advance_refuses_capture_at_once() {
    let root_dir = scratch("stop-advance");
    let root = root_dir.path().join("sessions");
    let stop = StopSignal::new();
    stop.stop();

    let began = Instant::now();
    let error = session::run_with_stop(
        &root,
        source(tone(30.0)),
        None,
        Transcription::Disabled,
        Duration::from_secs(3_600),
        stop,
    )
    .await
    .unwrap_err();
    assert!(error.contains("cancelled before capture setup"), "{error}");
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "pre-cancelled Start created a recording"
    );

    assert!(
        began.elapsed() < Duration::from_secs(10),
        "a pre-tripped signal was missed — the wait races the store"
    );
}
