//! Local transcription is gated by a speech detector (#242). Without that
//! detector, music and noise become text, so a local Start without it is
//! refused before any audio tap opens, and says how to fix it.
//!
//! The Whisper model here is a metadata-only fixture: admission resolves its
//! identity but never loads it, so no real weights are needed.
#![cfg(target_os = "macos")]

use fotw_web::{RecorderControl, RecordingState};
use fotwd::{recording::DaemonRecorder, stt};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

/// Start a local-only recording over `data_root` and report the error and how
/// many times capture was opened.
async fn start_local(data_root: &Path) -> (String, usize) {
    let model = data_root.join("ggml-fixture-asr.bin");
    std::fs::write(&model, b"metadata-only fixture; never native weights").unwrap();
    let settings = stt::SttSettings {
        engine: stt::SttEngine::Local,
        model: model.to_string_lossy().into_owned(),
        language: "en".into(),
    };
    let opens = Arc::new(AtomicUsize::new(0));
    let observed = opens.clone();
    let root = data_root.to_owned();
    let recorder = Arc::new(DaemonRecorder::with_parts(
        data_root.join("library/sessions"),
        tokio::runtime::Handle::current(),
        Default::default(),
        Default::default(),
        Box::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Err("capture opened".into())
        }),
        Box::new(move || stt::local_transcription(&settings, &root, "fixture", false, false)),
        Box::new(|_, _| panic!("refused admission must not produce a recording")),
        Duration::from_secs(1),
        Duration::from_secs(5),
    ));
    let requested = recorder.clone();
    let error = tokio::task::spawn_blocking(move || requested.start())
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    assert_eq!(recorder.status().state, RecordingState::Idle);
    (error, opens.load(Ordering::SeqCst))
}

fn assert_actionable(error: &str) {
    assert!(error.contains(stt::VAD_MODEL.name), "{error}");
    assert!(
        error.contains(&format!("fotwd stt models install {}", stt::VAD_MODEL.name)),
        "the refusal must say how to fix it: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_speech_detector_refuses_local_capture_with_the_fix() {
    let root = tempfile::tempdir().unwrap();
    let (error, opens) = start_local(root.path()).await;
    assert_eq!(
        opens, 0,
        "capture opened without a speech detector: {error}"
    );
    assert!(error.contains("missing"), "{error}");
    assert_actionable(&error);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_speech_detector_refuses_local_capture_with_the_fix() {
    let root = tempfile::tempdir().unwrap();
    let detector = stt::vad_model_path(root.path());
    std::fs::create_dir_all(detector.parent().unwrap()).unwrap();
    // Right size, wrong bytes: a corrupted or substituted file.
    std::fs::write(&detector, vec![0u8; stt::VAD_MODEL.bytes as usize]).unwrap();
    let (error, opens) = start_local(root.path()).await;
    assert_eq!(opens, 0, "capture opened with a corrupt detector: {error}");
    assert!(error.contains("not the published"), "{error}");
    assert_actionable(&error);
}

#[test]
fn the_detector_is_pinned_to_an_immutable_mit_revision() {
    let vad = stt::VAD_MODEL;
    assert_eq!(vad.license, "MIT");
    assert_eq!(vad.revision.len(), 40);
    assert_eq!(vad.sha256.len(), 64);
    assert_eq!(
        vad.url(),
        format!(
            "https://huggingface.co/ggml-org/whisper-vad/resolve/{}/ggml-silero-v5.1.2.bin",
            vad.revision
        )
    );
    // Installable by name, never selectable as a transcription model.
    assert_eq!(stt::catalog_model("silero-v5.1.2"), Some(&stt::VAD_MODEL));
    assert_eq!(stt::known_model("silero-v5.1.2"), None);
}
