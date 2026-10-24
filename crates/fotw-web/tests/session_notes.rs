#![cfg(feature = "store")]
mod common;
use fotw_store::{Db, DbKey, NewMeeting};
use fotw_web::{RecorderControl, RecorderError, RecordingStatus, StoreSource, WebServer};
use std::sync::{Arc, Mutex};
struct Recorder(Mutex<RecordingStatus>);
impl RecorderControl for Recorder {
    fn status(&self) -> RecordingStatus {
        self.0.lock().unwrap().clone()
    }
    fn start(&self) -> Result<RecordingStatus, RecorderError> {
        Ok(self.status())
    }
    fn stop(&self) -> Result<RecordingStatus, RecorderError> {
        Ok(self.status())
    }
}
#[tokio::test]
async fn session_notes_require_auth_and_follow_the_original_clock_through_stop() {
    let dir = std::env::temp_dir().join(format!("fotw-session-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fixture.sqlite3");
    let key = DbKey::from_bytes([73; 32]);
    let db = Db::open(&path, &key).unwrap();
    let mut finalizer = Db::open(&path, &key).unwrap();
    let recorder = Arc::new(Recorder(Mutex::new(
        RecordingStatus::recording(1000, 500).with_session_id(Some("session-A".into())),
    )));
    let server =
        WebServer::bind_with_recorder(0, Arc::new(StoreSource::new(db)), Some(recorder.clone()))
            .await
            .unwrap();
    let state = server.state().clone();
    let h = common::Harness {
        addr: server.addr(),
        token: state.policy().secret().expose_hex(),
        authority: state.policy().authority().to_owned(),
        origin: state.policy().origin().to_owned(),
        state,
    };
    let task = tokio::spawn(server.serve());
    let route = "/api/sessions/session-A/notes";
    let body = r#"{"revision":0,"body_md":"fixture decision","anchors":[{"id":"block-A","block_idx":0,"block_text":"fixture decision","typed_at_ms":250}]}"#;
    assert_eq!(h.get(route, &h.anonymous()).await.status, 404);
    assert_eq!(h.post(route, &h.anonymous(), Some(body)).await.status, 404);
    assert_eq!(h.get(route, &h.authorised()).await.status, 200);
    assert_eq!(
        h.post("/api/sessions/wrong/notes", &h.authorised(), Some(body))
            .await
            .status,
        404
    );
    assert_eq!(
        h.post(route, &h.authorised(), Some(&body.replace("250", "501")))
            .await
            .status,
        422
    );
    assert_eq!(h.post(route, &h.authorised(), Some(body)).await.status, 200);
    assert_eq!(h.post(route, &h.authorised(), Some(body)).await.status, 409);
    *recorder.0.lock().unwrap() = RecordingStatus::idle();
    // During the persistence gap, durable drafts remain readable; no new clock is guessed.
    assert_eq!(h.get(route, &h.authorised()).await.status, 200);
    let edited = body
        .replace("\"revision\":0", "\"revision\":1")
        .replace("fixture decision", "edited fixture");
    assert_eq!(
        h.post(route, &h.authorised(), Some(&edited)).await.status,
        409
    );
    let mut meeting = NewMeeting::new("fixture", "UTC");
    meeting.started_at_ms = 1000;
    let id = finalizer
        .finalize_capture("session-A", meeting, 1500, None)
        .unwrap();
    *recorder.0.lock().unwrap() =
        RecordingStatus::recording(9000, 9999).with_session_id(Some("session-B".into()));
    let saved = h.post(route, &h.authorised(), Some(&edited)).await;
    assert_eq!(saved.status, 200);
    let value: serde_json::Value = serde_json::from_str(&saved.body).unwrap();
    assert_eq!(value["meeting_id"], id);
    assert_eq!(value["elapsed_ms"], 500);
    assert_eq!(value["note"]["revision"], 2);
    let invalid = edited
        .replace("\"revision\":1", "\"revision\":2")
        .replace("250", "600");
    assert_eq!(
        h.post(route, &h.authorised(), Some(&invalid)).await.status,
        422
    );
    assert_eq!(
        finalizer.note_document(&id).unwrap().body_md,
        "edited fixture"
    );
    let mut hostile = h.authorised();
    hostile.iter_mut().find(|(k, _)| k == "Origin").unwrap().1 = "https://other.example".into();
    assert_eq!(h.post(route, &hostile, Some(body)).await.status, 404);
    let forged = body.replace("\"revision\":0", "\"elapsed_ms\":99999,\"revision\":0");
    assert_eq!(
        h.post(route, &h.authorised(), Some(&forged)).await.status,
        404
    );
    // The first autosave may arrive only after finalization; no draft is required.
    let mut empty = NewMeeting::new("fixture", "UTC");
    empty.started_at_ms = 1000;
    let empty_id = finalizer
        .finalize_capture("session-empty", empty, 1500, None)
        .unwrap();
    let fresh = body.replace("block-A", "block-fresh");
    let first = h
        .post(
            "/api/sessions/session-empty/notes",
            &h.authorised(),
            Some(&fresh),
        )
        .await;
    assert_eq!(first.status, 200);
    assert_eq!(finalizer.note_document(&empty_id).unwrap().revision, 1);
    task.abort();
}
