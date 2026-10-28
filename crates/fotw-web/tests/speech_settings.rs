//! Recording policy must be explicit, authenticated, and fail without rewriting it.
mod common;
use fotw_web::speech::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Debug)]
struct Fake {
    stored: Mutex<SpeechSettingsDoc>,
    writes: AtomicUsize,
}
impl SpeechControl for Fake {
    fn read(&self) -> SpeechSettingsResponse {
        SpeechSettingsResponse {
            settings: Some(self.stored.lock().unwrap().clone()),
            model_availability: ModelAvailability::PresentUnchecked,
            model_path: Some("/fixture/model.bin".into()),
            error: None,
            captions_installed: false,
        }
    }
    fn set(&self, s: SpeechSettingsDoc) -> Result<(), String> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if s.model == "/bad.bin" {
            return Err("model checksum mismatch".into());
        }
        *self.stored.lock().unwrap() = s;
        Ok(())
    }
}
async fn rig() -> (common::Harness, Arc<Fake>) {
    let h = common::start().await;
    let control = Arc::new(Fake {
        stored: Mutex::new(SpeechSettingsDoc {
            engine: SpeechMode::Local,
            model: "/fixture/model.bin".into(),
            language: "en".into(),
            live_captions: true,
        }),
        writes: AtomicUsize::new(0),
    });
    h.state.set_speech(control.clone());
    (h, control)
}
#[tokio::test]
async fn speech_routes_require_authentication_before_reading_or_writing() {
    let (h, c) = rig().await;
    for response in [
        h.get("/api/settings/speech", &h.anonymous()).await,
        h.post(
            "/api/settings/speech",
            &h.anonymous(),
            Some(r#"{"engine":"deepgram"}"#),
        )
        .await,
    ] {
        assert_eq!(response.status, 404);
        assert!(response.body.is_empty());
    }
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    assert_eq!(c.read().settings.unwrap().engine, SpeechMode::Local);
}
#[tokio::test]
async fn missing_or_unknown_policy_is_refused_without_calling_the_control() {
    let (h, c) = rig().await;
    for body in [
        r#"{}"#,
        r#"{"engine":"unknown"}"#,
        r#"{"engine":"deepgram","extra":true}"#,
    ] {
        let response = h
            .post("/api/settings/speech", &h.authorised(), Some(body))
            .await;
        assert_eq!(response.status, 404);
        assert!(response.body.is_empty());
    }
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn refused_backup_returns_original_local_choice_and_actionable_error() {
    let (h, c) = rig().await;
    let response = h
        .post(
            "/api/settings/speech",
            &h.authorised(),
            Some(r#"{"engine":"fallback","model":"/bad.bin"}"#),
        )
        .await;
    assert_eq!(response.status, 200);
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["settings"]["engine"], "local");
    assert_eq!(body["error"], "model checksum mismatch");
    assert_eq!(c.read().settings.unwrap().engine, SpeechMode::Local);
}
#[tokio::test]
async fn explicit_policy_save_is_normalized_and_read_back_with_security_headers() {
    let (h, c) = rig().await;
    let response = h
        .post(
            "/api/settings/speech",
            &h.authorised(),
            Some(r#"{"engine":"fallback","model":" /fixture/model.bin ","language":" AUTO "}"#),
        )
        .await;
    assert_eq!(response.status, 200);
    assert!(response.header("content-security-policy").is_some());
    let response = h.get("/api/settings/speech", &h.authorised()).await;
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["settings"]["engine"], "fallback");
    assert_eq!(body["settings"]["language"], "");
    assert_eq!(c.read().settings.unwrap().model, "/fixture/model.bin");
}
#[tokio::test]
async fn absent_control_is_indistinguishable_from_missing_route() {
    let h = common::start().await;
    let absent = h.get("/api/settings/speech", &h.authorised()).await;
    let missing = h.get("/api/no-such-route", &h.authorised()).await;
    assert_eq!(absent.bytes_without_date(), missing.bytes_without_date());
}
