//! The composition root, exercised end to end with no device and no network.
//!
//! `FileAudioSource` stands in for the tap, so this is the same code path a
//! real meeting takes — ring, pump, resampler, WAL — minus Core Audio. It is
//! what makes the integration testable on a Linux CI runner.

use std::time::Duration;

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, SampleFormat, StreamFormat, TapId};
use fotw_pipeline::wal::{SessionState, recover};
use fotwd::session::{self, Transcription};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fotwd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `seconds` of a 440 Hz tone at 48 kHz stereo — what the tap actually gives.
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

fn silence(seconds: f32) -> WavData {
    let format = StreamFormat::new(48_000, 2, SampleFormat::I16);
    WavData {
        format,
        samples: vec![0.0; (48_000.0 * seconds) as usize * 2],
    }
}

fn source(data: WavData) -> Box<dyn AudioTap> {
    Box::new(FileAudioSource::from_wav(
        TapId::system_default(),
        data,
        // Real-time pacing: the session is driven by a wall-clock duration, so
        // replaying faster would just finish early and prove less.
        ReplaySpeed::Realtime,
    ))
}

#[tokio::test]
async fn a_session_captures_audio_to_a_finalized_wal() {
    let root = tmpdir("capture");
    let outcome = session::run(
        &root,
        source(tone(3.0)),
        None,
        Transcription::Disabled,
        Duration::from_millis(1_200),
    )
    .await
    .unwrap();

    assert!(outcome.total_buffers > 0, "the tap delivered nothing");
    assert!(
        outcome.captured_audio(),
        "a 440 Hz tone must not read as silence"
    );

    // Roughly a second of 48 kHz stereo. Generous bounds: this is a
    // wall-clock test, and asserting an exact count would make it flaky
    // rather than correct.
    assert!(
        outcome.system_samples > 48_000,
        "expected at least ~0.5s of stereo samples, got {}",
        outcome.system_samples
    );

    let state = SessionState::read(&outcome.dir).unwrap();
    assert!(
        state.is_finalized(),
        "a session that ended normally must be finalized, not left recoverable"
    );
    assert_eq!(state.manifest.sample_rate_hz, 48_000);
    assert_eq!(state.manifest.channels, 2);
    assert!(state.system_seconds() > 0.5);
}

/// Recording must not depend on transcription being configured. This is the
/// whole reason raw audio is written before anything derived: a meeting is
/// never lost because a key expired or the network was down.
#[tokio::test]
async fn recording_works_with_no_provider_configured() {
    let root = tmpdir("noprovider");
    let outcome = session::run(
        &root,
        source(tone(2.0)),
        None,
        Transcription::Disabled,
        Duration::from_millis(900),
    )
    .await
    .unwrap();

    assert!(outcome.system_samples > 0);
    assert!(
        outcome.segments.is_empty(),
        "no provider means no transcript, not a failure"
    );

    // The audio is on disk and can be transcribed later.
    let pcm = std::fs::metadata(outcome.dir.join("system.pcm")).unwrap();
    assert!(pcm.len() > 0);
}

#[tokio::test]
async fn a_silent_source_is_reported_as_silent_rather_than_as_success() {
    let root = tmpdir("silent");
    let outcome = session::run(
        &root,
        source(silence(2.0)),
        None,
        Transcription::Disabled,
        Duration::from_millis(900),
    )
    .await
    .unwrap();

    assert!(outcome.total_buffers > 0, "buffers still arrived");
    assert!(
        !outcome.captured_audio(),
        "every buffer was digitally silent and the outcome must say so — \
         this is what distinguishes 'quiet room' from 'permission denied', \
         which macOS reports identically"
    );
    assert_eq!(outcome.silent_buffers, outcome.total_buffers);
}

#[tokio::test]
async fn a_finished_session_is_not_offered_for_recovery() {
    let root = tmpdir("recovery");
    let outcome = session::run(
        &root,
        source(tone(2.0)),
        None,
        Transcription::Disabled,
        Duration::from_millis(800),
    )
    .await
    .unwrap();

    let recoverable = recover(&root).unwrap();
    assert!(
        !recoverable.iter().any(|s| s.dir == outcome.dir),
        "a cleanly closed session must not appear in the recovery list"
    );
}

#[tokio::test]
async fn both_legs_are_recorded_as_separate_files() {
    let root = tmpdir("twolegs");
    let outcome = session::run(
        &root,
        source(tone(2.0)),
        Some(source(tone(2.0))),
        Transcription::Disabled,
        Duration::from_millis(900),
    )
    .await
    .unwrap();

    assert!(outcome.system_samples > 0, "system leg empty");
    assert!(outcome.mic_samples > 0, "mic leg empty");

    let state = SessionState::read(&outcome.dir).unwrap();
    assert!(state.system_frames > 0);
    assert!(state.mic_frames > 0);

    // Never pre-mixed: two files, two independent streams. This is what makes
    // "me vs them" free rather than a diarization problem.
    assert!(outcome.dir.join("system.pcm").exists());
    assert!(outcome.dir.join("mic.pcm").exists());
}

#[test]
fn transcript_text_joins_segments_and_skips_blanks() {
    use fotw_stt::{Source, TimestampSource, TranscriptSegment};

    let seg = |text: &str, start: u64| TranscriptSegment {
        id: format!("s{start}"),
        session_id: "t".into(),
        source: Source::System,
        speaker: None,
        text: text.into(),
        start_ms: start,
        end_ms: start + 100,
        words: Vec::new(),
        confidence: None,
        language: None,
        is_final: true,
        revision: 0,
        provider: "test".into(),
        model: "test".into(),
        timestamp_source: TimestampSource::Provider,
    };

    let outcome = fotwd::SessionOutcome {
        segments: vec![seg("hello", 0), seg("   ", 100), seg("world", 200)],
        ..Default::default()
    };
    assert_eq!(outcome.transcript_text(), "hello world");
}
