//! A whole session transcribed on-device (#33), with a scripted recognizer in
//! place of whisper.cpp so the run needs no weights.
//!
//! The stream's own behaviour is covered in `fotw-stt/tests/local_stream.rs`.
//! This is the wiring: `Transcription::Local` opens a leg on the WAL's session
//! id, the stop path waits for the model to finish what it heard, and a model
//! that will not load reaches the same error channel a Deepgram failure does
//! while the audio is kept.

use std::sync::Arc;
use std::time::Duration;

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, SampleFormat, StreamFormat, TapId};
use fotw_stt::local::PROVIDER;
use fotw_stt::{Recognized, Recognizer, RecognizerFactory, SttError, SttErrorClass};
use fotwd::session::{self, LocalLegs, SessionControl, Transcription};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("fotwd-local-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("sessions")).unwrap();
    base
}

/// Speech to the endpointer from the first frame: loud, and swinging 20 dB
/// every 120 ms the way syllables do, since a steady tone is dropped as noise.
fn tone(seconds: f32) -> WavData {
    let format = StreamFormat::new(48_000, 2, SampleFormat::I16);
    let n = (48_000.0 * seconds) as usize;
    let mut samples = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / 48_000.0;
        let level = if ((t / 0.12) as u32).is_multiple_of(2) {
            0.5
        } else {
            0.05
        };
        let v = (t * 440.0 * std::f32::consts::TAU).sin() * level;
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

struct Scripted;

impl Recognizer for Scripted {
    fn transcribe(&mut self, _pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        Ok(vec![Recognized {
            start_ms: 0,
            end_ms: 400,
            text: "heard locally".into(),
        }])
    }
}

struct Engine {
    loads: bool,
}

impl RecognizerFactory for Engine {
    fn model(&self) -> &str {
        "scripted"
    }

    fn language(&self) -> Option<&str> {
        None
    }

    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        if self.loads {
            Ok(Box::new(Scripted))
        } else {
            Err(SttError::new(
                SttErrorClass::Unsupported,
                PROVIDER,
                "the local transcription model is missing",
            ))
        }
    }
}

async fn record(name: &str, loads: bool) -> session::SessionOutcome {
    let root = tmpdir(name);
    let control = SessionControl::new();
    let stop = control.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        stop.stop();
    });
    let legs = LocalLegs::for_session(Arc::new(Engine { loads }), "placeholder", false, true);
    session::run_with_control(
        &root.join("sessions"),
        file_tap(),
        None,
        Transcription::Local(legs),
        Duration::from_secs(30),
        control,
    )
    .await
    .expect("session runs")
}

#[tokio::test]
async fn a_local_session_is_transcribed_by_the_time_it_returns() {
    let outcome = record("ok", true).await;

    // The tone never pauses, so the only utterance is the one still open at
    // Stop: it exists only because the stop path waited for the model.
    assert_eq!(outcome.segments.len(), 1, "{:?}", outcome.segments);
    let segment = &outcome.segments[0];
    assert_eq!(segment.text, "heard locally");
    assert_eq!(segment.provider, PROVIDER);
    assert_eq!(segment.model, "scripted");
    // The WAL's id, not the factory's placeholder.
    assert_eq!(
        segment.session_id,
        outcome.dir.file_name().unwrap().to_string_lossy()
    );
    assert!(outcome.stt_errors.is_empty(), "{:?}", outcome.stt_errors);
}

#[tokio::test]
async fn a_model_that_will_not_load_keeps_the_audio_and_says_why() {
    let outcome = record("missing", false).await;

    assert!(outcome.system_samples > 0, "the meeting is still recorded");
    assert!(outcome.segments.is_empty());
    assert_eq!(outcome.stt_errors.len(), 1, "{:?}", outcome.stt_errors);
    assert!(
        outcome.stt_errors[0].contains("model is missing"),
        "{:?}",
        outcome.stt_errors
    );
}
