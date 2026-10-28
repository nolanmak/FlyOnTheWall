//! Model admission must finish before any tap is opened or any clock claims capture.
use fotw_web::{RecorderControl, RecorderError, RecordingState};
use fotwd::{recording::DaemonRecorder, session::Transcription};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_backup_is_reported_before_opening_capture() {
    let root = tempfile::tempdir().unwrap();
    let opens = Arc::new(AtomicUsize::new(0));
    let observed = opens.clone();
    let recorder = Arc::new(DaemonRecorder::with_parts(
        root.path().join("sessions"),
        tokio::runtime::Handle::current(),
        Default::default(),
        Default::default(),
        Box::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Err("capture must remain closed".into())
        }),
        Box::new(|| {
            Transcription::PreparingFailover(Box::pin(async {
                Err("offline backup model unavailable".into())
            }))
        }),
        Box::new(|_, _| panic!("failed admission must not produce a recording")),
        Duration::from_secs(1),
        Duration::from_secs(2),
    ));
    let requested = recorder.clone();
    let error = tokio::task::spawn_blocking(move || requested.start())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("offline backup model unavailable"),
        "{error}"
    );
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    assert_eq!(recorder.status().state, RecordingState::Idle);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_keeps_model_preparation_owned_and_never_starts_capture_later() {
    let root = tempfile::tempdir().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let opens = Arc::new(AtomicUsize::new(0));
    let observed = opens.clone();
    let recorder = Arc::new(DaemonRecorder::with_parts(
        root.path().join("sessions"),
        tokio::runtime::Handle::current(),
        Default::default(),
        Default::default(),
        Box::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Err("capture must remain closed".into())
        }),
        Box::new(move || {
            let gate = gate.clone();
            Transcription::PreparingFailover(Box::pin(async move {
                gate.notified().await;
                Err("released model preparation".into())
            }))
        }),
        Box::new(|_, _| panic!("cancelled admission must not persist")),
        Duration::from_secs(1),
        Duration::from_millis(40),
    ));
    let requested = recorder.clone();
    let error = tokio::task::spawn_blocking(move || requested.start())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("model"), "{error}");
    let status = recorder.status();
    assert_eq!(status.state, RecordingState::Preparing);
    assert!(!status.is_recording());
    assert!(status.preparation_cancel_requested);
    assert_eq!(status.started_at_ms, None);
    assert_eq!(status.auto_stop_at_ms, None);
    assert!(matches!(
        recorder.start(),
        Err(RecorderError::AlreadyRecording)
    ));
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while recorder.status().state != RecordingState::Idle {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        opens.load(Ordering::SeqCst),
        0,
        "a timed-out preparation started late capture"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_fallback_factory_rejects_missing_model_before_cloud_or_capture() {
    let root = tempfile::tempdir().unwrap();
    let data_root = root.path().to_owned();
    let cloud_calls = Arc::new(AtomicUsize::new(0));
    let capture_calls = Arc::new(AtomicUsize::new(0));
    let observe_cloud = cloud_calls.clone();
    let observe_capture = capture_calls.clone();
    let recorder = Arc::new(DaemonRecorder::with_parts(
        root.path().join("sessions"),
        tokio::runtime::Handle::current(),
        Default::default(),
        Default::default(),
        Box::new(move || {
            observe_capture.fetch_add(1, Ordering::SeqCst);
            Err("unexpected capture".into())
        }),
        Box::new(move || {
            let observe_cloud = observe_cloud.clone();
            fotwd::stt::fallback_transcription(
                fotwd::stt::SttSettings {
                    engine: fotwd::stt::SttEngine::Fallback,
                    model: data_root
                        .join("missing-model.bin")
                        .to_string_lossy()
                        .into_owned(),
                    language: "en".into(),
                },
                data_root.clone(),
                Box::new(move || {
                    observe_cloud.fetch_add(1, Ordering::SeqCst);
                    Err("unexpected cloud lookup".into())
                }),
            )
        }),
        Box::new(|_, _| panic!("failed admission must not persist")),
        Duration::from_secs(1),
        Duration::from_secs(5),
    ));
    let requested = recorder.clone();
    let error = tokio::task::spawn_blocking(move || requested.start())
        .await
        .unwrap()
        .unwrap_err();
    assert!(!error.to_string().contains("timed out"), "{error}");
    assert_eq!(cloud_calls.load(Ordering::SeqCst), 0);
    assert_eq!(capture_calls.load(Ordering::SeqCst), 0);
    assert_eq!(recorder.status().state, RecordingState::Idle);
}

struct CountingFactory(Arc<AtomicUsize>);
struct CountedState(Arc<AtomicUsize>);
impl Drop for CountedState {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl fotw_stt::Recognizer for CountedState {
    fn transcribe(&mut self, _: &[f32]) -> Result<Vec<fotw_stt::Recognized>, fotw_stt::SttError> {
        panic!("cancelled preparation must not infer audio")
    }
}
impl fotw_stt::RecognizerFactory for CountingFactory {
    fn model(&self) -> &str {
        "fixture"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn fotw_stt::Recognizer>, fotw_stt::SttError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CountedState(self.0.clone())))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_retains_both_native_states_until_late_success_is_discarded() {
    let root = tempfile::tempdir().unwrap();
    let states = Arc::new(AtomicUsize::new(0));
    let count = states.clone();
    let capture_calls = Arc::new(AtomicUsize::new(0));
    let observe_capture = capture_calls.clone();
    let prepared = Arc::new(tokio::sync::Notify::new());
    let announce = prepared.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let recorder = Arc::new(DaemonRecorder::with_parts(
        root.path().join("sessions"),
        tokio::runtime::Handle::current(),
        Default::default(),
        Default::default(),
        Box::new(move || {
            observe_capture.fetch_add(1, Ordering::SeqCst);
            Err("unexpected capture".into())
        }),
        Box::new(move || {
            let factory: Arc<dyn fotw_stt::RecognizerFactory> =
                Arc::new(CountingFactory(count.clone()));
            let announce = announce.clone();
            let gate = gate.clone();
            Transcription::PreparingFailover(Box::pin(async move {
                let config = |source| {
                    fotwd::failover::stream::FailoverConfig::new(
                        fotw_stt::DeepgramStreamConfig::new(
                            "fixture-not-a-key",
                            fotw_stt::deepgram::DeepgramConfig::new("placeholder", source),
                        ),
                        factory.clone(),
                    )
                };
                let legs = fotwd::session::FailoverLegs::prepare(
                    config(fotw_stt::Source::System),
                    Some(config(fotw_stt::Source::Mic)),
                )
                .await
                .map_err(|e| e.message)?;
                announce.notify_one();
                gate.notified().await;
                Ok(legs)
            }))
        }),
        Box::new(|_, _| panic!("cancelled admission must not persist")),
        Duration::from_secs(1),
        Duration::from_secs(5),
    ));
    let requested = recorder.clone();
    let starting = tokio::task::spawn_blocking(move || requested.start());
    tokio::time::timeout(Duration::from_secs(3), prepared.notified())
        .await
        .unwrap();
    assert_eq!(states.load(Ordering::SeqCst), 2);
    assert_eq!(recorder.status().state, RecordingState::Preparing);
    assert!(recorder.stop().unwrap().preparation_cancel_requested);
    assert!(starting.await.unwrap().is_err());
    assert_eq!(states.load(Ordering::SeqCst), 2);
    assert!(matches!(
        recorder.start(),
        Err(RecorderError::AlreadyRecording)
    ));
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        while recorder.status().state != RecordingState::Idle {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(states.load(Ordering::SeqCst), 0);
    assert_eq!(capture_calls.load(Ordering::SeqCst), 0);
}

/// Explicit public-weight admission smoke: no capture, credentials or provider.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires cached published small.en weights and the Silero VAD model"]
async fn installed_backup_prepares_both_sources_without_cloud_or_capture() {
    let model = std::path::PathBuf::from(
        std::env::var_os("FOTW_NATIVE_HANDOFF_MODEL").expect("set FOTW_NATIVE_HANDOFF_MODEL"),
    );
    let checked = model.clone();
    tokio::task::spawn_blocking(move || {
        let known = fotwd::stt::verify_model(&checked)
            .unwrap()
            .expect("published weights only");
        assert_eq!(known.name, "small.en");
    })
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let root = tempfile::tempdir().unwrap();
    // The backup is refused without the speech detector (#242).
    let detector = fotwd::stt::vad_model_path(root.path());
    std::fs::create_dir_all(detector.parent().unwrap()).unwrap();
    std::fs::copy(
        std::env::var_os("FOTW_SILERO_VAD_MODEL")
            .expect("set FOTW_SILERO_VAD_MODEL to ggml-silero-v5.1.2.bin"),
        &detector,
    )
    .unwrap();
    let transcription = fotwd::stt::fallback_transcription(
        fotwd::stt::SttSettings {
            engine: fotwd::stt::SttEngine::Fallback,
            model: model.to_string_lossy().into_owned(),
            language: "en".into(),
        },
        root.path().to_owned(),
        Box::new(move || {
            let mut legs = fotwd::session::DeepgramLegs::for_session(
                "fixture-not-a-key",
                "placeholder",
                true,
                true,
            );
            legs.system.endpoint = fotw_stt::DeepgramEndpoint::loopback(port);
            legs.mic.as_mut().unwrap().endpoint = fotw_stt::DeepgramEndpoint::loopback(port);
            Ok(legs)
        }),
    );
    let Transcription::PreparingFailover(preparation) = transcription else {
        panic!("fallback must defer native preparation");
    };
    let legs = tokio::time::timeout(Duration::from_secs(30), preparation)
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    drop(legs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_capture_setup_reports_possible_audio_and_retains_ownership() {
    let root = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let announce = entered.clone();
    let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = gate.clone();
    let recorder = Arc::new(DaemonRecorder::with_parts(
        root.path().join("sessions"),
        tokio::runtime::Handle::current(),
        Default::default(),
        Default::default(),
        Box::new(move || {
            announce.notify_one();
            let (lock, wake) = &*gate;
            let ready = lock.lock().unwrap();
            let _ready = wake.wait_while(ready, |released| !*released).unwrap();
            Err("fixture capture opener released".into())
        }),
        Box::new(|| Transcription::Disabled),
        Box::new(|_, _| panic!("fixture opens no real capture")),
        Duration::from_secs(1),
        Duration::from_secs(5),
    ));
    let requested = recorder.clone();
    let starting = tokio::task::spawn_blocking(move || requested.start());
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    assert_eq!(recorder.status().state, RecordingState::Starting);
    let stopped = recorder.stop().unwrap();
    assert_eq!(stopped.state, RecordingState::Starting);
    assert!(stopped.preparation_cancel_requested);
    assert!(
        stopped
            .transcription_error
            .unwrap()
            .contains("buffered audio")
    );
    assert!(starting.await.unwrap().is_err());
    assert!(matches!(
        recorder.start(),
        Err(RecorderError::AlreadyRecording)
    ));
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    tokio::time::timeout(Duration::from_secs(3), async {
        while recorder.status().state != RecordingState::Idle {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
