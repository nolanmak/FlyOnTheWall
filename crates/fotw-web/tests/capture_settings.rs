//! Capture preferences use the normal authenticated settings ingress.
mod common;
use fotw_web::capture::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Debug)]
struct Fake {
    stored: Mutex<CaptureSettingsDoc>,
    writes: AtomicUsize,
}
impl CaptureControl for Fake {
    fn read(&self) -> CaptureSettingsResponse {
        CaptureSettingsResponse {
            settings: Some(*self.stored.lock().unwrap()),
            supported: false,
            error: None,
        }
    }
    fn set(&self, doc: CaptureSettingsDoc) -> Result<(), String> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if doc.filtered() {
            return Err("Audio exclusions require macOS 26 or newer".into());
        }
        *self.stored.lock().unwrap() = doc;
        Ok(())
    }
}
async fn rig() -> (common::Harness, Arc<Fake>) {
    let h = common::start().await;
    let c = Arc::new(Fake {
        stored: Mutex::new(CaptureSettingsDoc::default()),
        writes: AtomicUsize::new(0),
    });
    h.state.set_capture(c.clone());
    (h, c)
}
#[tokio::test]
async fn unauthenticated_and_malformed_writes_cannot_change_capture_policy() {
    let (h, c) = rig().await;
    let valid = r#"{"ignore_spotify":true,"ignore_brave":false}"#;
    for response in [
        h.get("/api/settings/capture", &h.anonymous()).await,
        h.post("/api/settings/capture", &h.anonymous(), Some(valid))
            .await,
        h.post("/api/settings/capture", &h.authorised(), Some("{}"))
            .await,
        h.post(
            "/api/settings/capture",
            &h.authorised(),
            Some(r#"{"ignore_spotify":false,"ignore_brave":false,"ignore_chrome":true}"#),
        )
        .await,
    ] {
        assert_eq!(response.status, 404);
        assert!(response.body.is_empty());
    }
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn unsupported_policy_preserves_saved_choice_and_returns_actionable_error() {
    let (h, c) = rig().await;
    let response = h
        .post(
            "/api/settings/capture",
            &h.authorised(),
            Some(r#"{"ignore_spotify":true,"ignore_brave":false}"#),
        )
        .await;
    assert_eq!(response.status, 200);
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["settings"]["ignore_spotify"], false);
    assert!(body["error"].as_str().unwrap().contains("macOS 26"));
    assert_eq!(c.read().settings.unwrap(), CaptureSettingsDoc::default());
}
#[tokio::test]
async fn explicit_disabling_remains_available_on_unsupported_runtime() {
    let (h, c) = rig().await;
    *c.stored.lock().unwrap() = CaptureSettingsDoc {
        ignore_spotify: true,
        ignore_brave: true,
    };
    let response = h
        .post(
            "/api/settings/capture",
            &h.authorised(),
            Some(r#"{"ignore_spotify":false,"ignore_brave":false}"#),
        )
        .await;
    assert_eq!(response.status, 200);
    assert!(response.header("content-security-policy").is_some());
    assert_eq!(c.read().settings.unwrap(), CaptureSettingsDoc::default());
}
