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
    let session = outcome.dir.file_name().unwrap().to_string_lossy();
    let reader = fotw_pipeline::replay::ReplayReader::open(
        &outcome.dir,
        &session,
        fotw_pipeline::replay::ReplaySource::System,
    )
    .expect("local-only recording must preserve its exact canonical speech input");
    let saved = fotwd::failover::coverage::CoverageJournal::snapshot(
        &outcome.dir,
        &session,
        fotw_stt::Source::System,
    )
    .unwrap();
    assert_eq!(saved.epochs[0].engine.provider, PROVIDER);
    assert_eq!(saved.epochs[0].reason, None);
    assert_eq!(
        saved.commits.last().unwrap().through_sample,
        reader.saved_end().unwrap()
    );
    assert!(
        saved.fallback.is_some(),
        "local input needs its saved restart plan"
    );
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

#[tokio::test]
async fn cancelling_local_identity_preparation_waits_for_owned_work_and_never_starts_capture() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let notify = entered.clone();
    let waiting = release.clone();
    let transcription = Transcription::PreparingLocal(Box::pin(async move {
        notify.notify_one();
        waiting.notified().await;
        Ok(LocalLegs::for_session(
            Arc::new(Engine { loads: true }),
            "fixture",
            false,
            false,
        ))
    }));
    let control = SessionControl::new();
    let stop = control.stop.clone();
    let mut task = tokio::spawn(async move { transcription.prepare(&control).await });
    entered.notified().await;
    stop.stop();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut task)
            .await
            .is_err(),
        "owned preparation must remain accounted for after Cancel"
    );
    release.notify_one();
    assert!(task.await.unwrap().err().unwrap().contains("cancelled"));
}

/// Opt-in real local-only session. Hash guards allow only the approved cached
/// small.en model and public JFK fixture; no device or provider is opened.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires approved cached small.en/JFK assets"]
async fn installed_local_only_session_preserves_public_audio_and_its_verified_restart_identity() {
    use sha2::{Digest, Sha256};
    let model = std::path::PathBuf::from(
        std::env::var_os("FOTW_NATIVE_LOCAL_MODEL").expect("set FOTW_NATIVE_LOCAL_MODEL"),
    );
    let wav = std::path::PathBuf::from(
        std::env::var_os("FOTW_NATIVE_LOCAL_WAV").expect("set FOTW_NATIVE_LOCAL_WAV"),
    );
    assert_eq!(std::fs::metadata(&wav).unwrap().len(), 352078);
    let bytes = std::fs::read(wav).unwrap();
    assert_eq!(
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        "59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e"
    );
    let checked = model.clone();
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            fotwd::stt::verify_model(&checked).unwrap().unwrap().name,
            "small.en"
        )
    })
    .await
    .unwrap();
    let decoded = fotw_audio::wav::parse(&bytes).unwrap();
    assert_eq!(decoded.frames(), 176000);
    let root = tempfile::tempdir().unwrap();
    let settings = fotwd::stt::SttSettings {
        engine: fotwd::stt::SttEngine::Local,
        model: model.to_string_lossy().into_owned(),
        language: "en".into(),
    };
    let transcription =
        fotwd::stt::local_transcription(&settings, root.path(), "placeholder", false, false);
    let began = std::time::Instant::now();
    let outcome = session::run_with_control(
        root.path(),
        Box::new(FileAudioSource::from_wav(
            TapId::system_default(),
            decoded,
            ReplaySpeed::Realtime,
        )),
        None,
        transcription,
        Duration::from_secs(12),
        SessionControl::new(),
    )
    .await
    .unwrap();
    assert!(outcome.stt_errors.is_empty(), "{:?}", outcome.stt_errors);
    let text = outcome.transcript_text();
    assert!(text.to_ascii_lowercase().contains("country"), "{text}");
    let id = outcome.dir.file_name().unwrap().to_string_lossy();
    let reader = fotw_pipeline::replay::ReplayReader::open(
        &outcome.dir,
        &id,
        fotw_pipeline::replay::ReplaySource::System,
    )
    .unwrap();
    assert_eq!(reader.saved_end().unwrap(), 176000);
    let saved = fotwd::failover::coverage::CoverageJournal::snapshot(
        &outcome.dir,
        &id,
        fotw_stt::Source::System,
    )
    .unwrap();
    let revision = "sha256:c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d";
    assert_eq!(
        saved.fallback.unwrap().engine.revision.as_deref(),
        Some(revision)
    );
    assert_eq!(saved.commits.last().unwrap().through_sample, 176000);
    eprintln!(
        "{}",
        serde_json::json!({"fixture":"approved-public-jfk","mode":"local-only","elapsed_ms":began.elapsed().as_millis(),"canonical_samples":176000,"revision":revision,"transcript":text,"tokens":fotw_stt::normalize_tokens(&text)})
    );
}
