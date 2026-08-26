//! Ending a session on demand rather than on a stopwatch.
//!
//! The CLI records for a fixed number of seconds, which is the right shape for
//! `fotwd record 3600`. A Start/Stop button is the other shape: nobody knows
//! how long the meeting is when it begins. Both must end the same way — taps
//! stopped, WAL finalized, transcript drained — so `run_with_stop` is the real
//! function and `run` is the delegate that passes a signal nobody holds.

use std::time::{Duration, Instant};

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, SampleFormat, StreamFormat, TapId};
use fotwd::session::{self, StopSignal, Transcription};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fotwd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
    let root = tmpdir("stop-early");
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
    let root = tmpdir("stop-duration");
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
async fn a_signal_stopped_in_advance_ends_it_at_once() {
    let root = tmpdir("stop-advance");
    let stop = StopSignal::new();
    stop.stop();

    let began = Instant::now();
    session::run_with_stop(
        &root,
        source(tone(30.0)),
        None,
        Transcription::Disabled,
        Duration::from_secs(3_600),
        stop,
    )
    .await
    .unwrap();

    assert!(
        began.elapsed() < Duration::from_secs(10),
        "a pre-tripped signal was missed — the wait races the store"
    );
}
