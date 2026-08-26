//! The composition root, exercised end to end with no device and no network.
//!
//! `FileAudioSource` stands in for the tap, so this is the same code path a
//! real meeting takes — ring, pump, resampler, WAL — minus Core Audio. It is
//! what makes the integration testable on a Linux CI runner.

use std::time::Duration;

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, SampleFormat, StreamFormat, TapId};
use fotw_pipeline::wal::{SessionState, TrackFormat, recover};
use fotwd::session::{self, LegAudio, Transcription};

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

    assert_eq!(
        outcome.system_buffers.audio(),
        LegAudio::Audible,
        "a 440 Hz tone must not read as silence: {:?}",
        outcome.system_buffers
    );
    assert!(outcome.captured_audio());
    assert_eq!(
        outcome.mic_buffers, None,
        "no mic tap was passed, so there is no mic leg to report on — which \
         is not the same as one that reported nothing"
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

    assert!(outcome.system_buffers.total > 0, "buffers still arrived");
    assert!(
        !outcome.captured_audio(),
        "every buffer was digitally silent and the outcome must say so — \
         this is what distinguishes 'quiet room' from 'permission denied', \
         which macOS reports identically"
    );
    assert_eq!(outcome.system_buffers.audio(), LegAudio::Silent);
    assert_eq!(outcome.system_buffers.silent, outcome.system_buffers.total);

    // And it says so where a human reads it. There is no mic leg here to
    // rescue the recording, so this is a fully silent meeting — but the line
    // still names the leg, because with a mic attached the same silence would
    // be a system-audio grant and nothing else (#79's channel, #81's leg).
    assert!(
        outcome
            .stt_errors
            .iter()
            .any(|e| e.starts_with("capture (system):")),
        "a leg that captured nothing audible must reach the degradation \
         channel: {:?}",
        outcome.stt_errors
    );
}

/// A dead microphone beside a working system tap (#81).
///
/// The mic sink used to be built with counters constructed inline, held by
/// nobody, so an hour of muted hardware — or a denied grant, which macOS
/// answers with silence rather than an error — was indistinguishable from an
/// hour of a working microphone. The two legs are reported separately here,
/// and this is the case where they disagree.
#[tokio::test]
async fn a_dead_mic_is_reported_separately_from_a_working_system_leg() {
    let root = tmpdir("deadmic");
    let outcome = session::run(
        &root,
        source(tone(2.0)),
        Some(source(silence(2.0))),
        Transcription::Disabled,
        Duration::from_millis(900),
    )
    .await
    .unwrap();

    let mic = outcome
        .mic_buffers
        .expect("the mic tap started, so it reports");
    assert!(mic.total > 0, "the mic tap delivered nothing at all");
    assert_eq!(
        (outcome.system_buffers.audio(), mic.audio()),
        (LegAudio::Audible, LegAudio::Silent),
        "the legs must disagree here: system {:?}, mic {:?}",
        outcome.system_buffers,
        mic
    );

    // The recording is still worth keeping — the far end is on disk — so the
    // one derived answer stays true. What changed is that "the far end was
    // captured" no longer doubles as a claim about the microphone.
    assert!(
        outcome.captured_audio(),
        "the system leg was live, so audio was captured"
    );
    assert!(
        outcome
            .stt_errors
            .iter()
            .any(|e| e.starts_with("capture (mic):")),
        "a dead mic is a degraded meeting and someone should know: {:?}",
        outcome.stt_errors
    );
}

/// The other direction: a note to self, with nothing playing.
///
/// A silent system leg is normal here — there is no far end — and the rule
/// this outcome derives must not call that a failed recording. Before #81 it
/// did: the single pair of counters *was* the system leg, so `captured_audio()`
/// was false and the CLI answered a perfectly good recording with the
/// screen-recording permission speech.
#[tokio::test]
async fn a_note_to_self_with_nothing_playing_is_not_a_capture_failure() {
    let root = tmpdir("notetoself");
    let outcome = session::run(
        &root,
        source(silence(2.0)),
        Some(source(tone(2.0))),
        Transcription::Disabled,
        Duration::from_millis(900),
    )
    .await
    .unwrap();

    let mic = outcome
        .mic_buffers
        .expect("the mic tap started, so it reports");
    assert_eq!(
        (outcome.system_buffers.audio(), mic.audio()),
        (LegAudio::Silent, LegAudio::Audible),
        "system {:?}, mic {:?}",
        outcome.system_buffers,
        mic
    );
    assert!(
        outcome.captured_audio(),
        "the user's own voice is audio, and this recording is worth keeping"
    );

    // Not a failure is not the same as unremarkable: the quiet leg is still
    // named, because the same shape is what a denied system-audio grant looks
    // like and only the user knows which of the two this was.
    assert!(
        outcome
            .stt_errors
            .iter()
            .any(|e| e.starts_with("capture (system):")),
        "{:?}",
        outcome.stt_errors
    );
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

/// `seconds` of an 880 Hz tone at 48 kHz **mono** — the ordinary mic shape,
/// and deliberately not the system tap's.
fn mono_tone(seconds: f32) -> WavData {
    let format = StreamFormat::new(48_000, 1, SampleFormat::I16);
    let n = (48_000.0 * seconds) as usize;
    let samples = (0..n)
        .map(|i| (i as f32 / 48_000.0 * 880.0 * std::f32::consts::TAU).sin() * 0.5)
        .collect();
    WavData { format, samples }
}

#[tokio::test]
async fn each_leg_records_its_own_format_so_a_mono_mic_is_not_read_as_stereo() {
    // #80 originated here: the WAL was created with the system tap's format
    // and the encoder applied it to both legs, so a mono mic WAL came back at
    // half its real length at 2× speed. The manifest has to carry what each
    // tap actually reported.
    let root = tmpdir("legformats");
    let outcome = session::run(
        &root,
        source(tone(2.0)),
        Some(source(mono_tone(2.0))),
        Transcription::Disabled,
        Duration::from_millis(900),
    )
    .await
    .unwrap();

    let state = SessionState::read(&outcome.dir).unwrap();
    assert_eq!(
        state.manifest.system_format,
        Some(TrackFormat::new(48_000, 2))
    );
    assert_eq!(state.manifest.mic_format, Some(TrackFormat::new(48_000, 1)));

    // Which makes the two legs the same length in frames — the same wall
    // clock, read at each leg's own frame size. Loose bounds because this is
    // a real-time replay and the mic tap starts second; a factor of two is
    // the bug, and nothing near it is timing jitter.
    let (sys, mic) = (state.system_frames, state.mic_frames);
    assert!(sys > 0 && mic > 0, "a leg is empty: {sys} / {mic}");
    let ratio = mic as f64 / sys as f64;
    assert!(
        (0.8..=1.2).contains(&ratio),
        "the mic leg is {ratio:.2}× the system leg ({mic} frames against \
         {sys}) for the same wall clock"
    );
}
