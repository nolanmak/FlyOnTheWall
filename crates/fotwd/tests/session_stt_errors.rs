//! Transcription failures must reach a human while the meeting is still on.
//!
//! # What this is for
//!
//! `session::run` used to consume only `StreamEvent::Final` and drop
//! `StreamEvent::Error` on the floor. Two separate bugs — a rejected handshake
//! and a frame the reader could not parse — each killed the Deepgram stream on
//! connect, and neither was visible anywhere: no log line, no field, no UI. An
//! empty `stt.jsonl` beside two hours of perfect audio looked exactly like a
//! meeting where nobody spoke.
//!
//! Both were fixed. This exists so the next one is noticed in seconds rather
//! than surviving the life of the project.

use std::time::Duration;

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, SampleFormat, StreamFormat, TapId};
use fotwd::session::{self, SessionControl, SttErrors, Transcription};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("fotwd-stt-{name}-{}", std::process::id()));
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

#[test]
fn a_fresh_error_channel_is_empty() {
    let e = SttErrors::new();
    assert!(e.latest().is_none());
    assert_eq!(e.count(), 0);
}

#[test]
fn recording_an_error_makes_it_readable() {
    let e = SttErrors::new();
    e.record("deepgram handshake returned HTTP 400");

    assert_eq!(
        e.latest().as_deref(),
        Some("deepgram handshake returned HTTP 400")
    );
    assert_eq!(e.count(), 1);
}

/// The UI shows the most recent one; a stream that retries and fails should
/// not bury the current reason under the first.
#[test]
fn the_latest_error_wins() {
    let e = SttErrors::new();
    e.record("first");
    e.record("second");

    assert_eq!(e.latest().as_deref(), Some("second"));
    assert_eq!(e.count(), 2);
}

/// Clones share state: the session task holds one end, the recorder the other.
#[test]
fn a_clone_shares_the_record() {
    let e = SttErrors::new();
    let held = e.clone();
    held.record("from the session task");

    assert_eq!(e.latest().as_deref(), Some("from the session task"));
}

/// A session with transcription off must not invent a failure — "off" and
/// "broken" are different states and the UI must not confuse them.
#[tokio::test]
async fn transcription_disabled_reports_no_error() {
    let root = tmpdir("disabled");
    let control = SessionControl::new();
    let errors = control.errors.clone();
    let stop = control.stop.clone();

    let held = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        held.stop();
    });

    let outcome = session::run_with_control(
        &root.join("sessions"),
        file_tap(),
        None,
        Transcription::Disabled,
        Duration::from_secs(30),
        control,
    )
    .await
    .expect("session runs");

    assert!(
        errors.latest().is_none(),
        "no provider, so no provider error"
    );
    assert!(outcome.stt_errors.is_empty());
}

/// The outcome carries what went wrong, so the CLI can print it after a run
/// even when nobody was watching a dashboard.
#[test]
fn the_outcome_carries_the_errors() {
    let e = SttErrors::new();
    e.record("could not parse a Deepgram frame");
    e.record("deepgram handshake returned HTTP 400");

    let drained = e.drain();
    assert_eq!(drained.len(), 2);
    assert!(drained[1].contains("400"));
}
