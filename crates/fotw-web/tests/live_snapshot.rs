use fotw_web::{Delta, DeltaHub};
fn delta(session: &str, idx: i64, start: i64) -> Delta {
    Delta {
        meeting_id: session.into(),
        idx,
        start_ms: start,
        end_ms: start + 1000,
        channel: "system".into(),
        text: "word".into(),
        is_final: true,
    }
}
#[test]
fn snapshot_recovers_unflushed_finals_without_mixing_sessions_or_duplicates() {
    let hub = DeltaHub::new();
    hub.publish(delta("A", 0, 17000));
    hub.publish(delta("A", 0, 17000));
    hub.publish(delta("B", 0, 3000));
    let a = hub.snapshot("A");
    assert_eq!(a.deltas.len(), 1);
    assert_eq!(a.speech_origin_ms, Some(17000));
    assert!(!a.truncated);
    assert_eq!(hub.snapshot("B").deltas[0].start_ms, 3000);
    assert!(hub.snapshot("missing").deltas.is_empty());
}
#[test]
fn bounded_history_retains_origin_and_reports_omitted_rows() {
    let hub = DeltaHub::new();
    for idx in 0..20001 {
        hub.publish(delta("A", idx, 17000 + idx * 1000));
        hub.flush();
    }
    let a = hub.snapshot("A");
    assert_eq!(a.deltas.len(), 20000);
    assert!(a.truncated);
    assert_eq!(a.speech_origin_ms, Some(17000));
}

mod common;
#[tokio::test]
async fn snapshot_endpoint_requires_authorization_and_keeps_session_boundaries() {
    let h = common::start().await;
    h.state.hub().publish(delta("A", 0, 17000));
    h.state.hub().publish(delta("B", 0, 36000));
    let path = "/api/recording/transcript?session_id=A";
    assert_eq!(h.get(path, &h.anonymous()).await.status, 404);
    let response = h.get(path, &h.authorised()).await;
    assert_eq!(response.status, 200);
    assert_eq!(
        h.get(
            "/api/recording/transcript?session_id=evicted",
            &h.authorised()
        )
        .await
        .status,
        404
    );
    let snapshot: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(snapshot["session_id"], "A");
    assert_eq!(snapshot["deltas"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["deltas"][0]["meeting_id"], "A");
}
#[test]
fn persistence_handoff_names_the_capture_and_survives_a_reconnect() {
    let hub = DeltaHub::new();
    hub.publish(delta("A", 0, 17000));
    hub.publish(delta("B", 0, 36000));
    hub.announce_session_ready(
        "saved-A",
        fotw_web::MeetingReadyReason::Persisted,
        Some("A"),
    );
    assert_eq!(
        hub.snapshot("A").saved_meeting_id.as_deref(),
        Some("saved-A")
    );
    assert!(hub.snapshot("B").saved_meeting_id.is_none());
}

#[tokio::test]
async fn an_active_session_before_first_speech_has_an_empty_successful_snapshot() {
    use fotw_web::{MemorySource, RecorderControl, RecorderError, RecordingStatus, WebServer};
    use std::sync::Arc;
    struct QuietRecorder;
    impl RecorderControl for QuietRecorder {
        fn status(&self) -> RecordingStatus {
            RecordingStatus::recording(1000, 0).with_session_id(Some("quiet".into()))
        }
        fn start(&self) -> Result<RecordingStatus, RecorderError> {
            Ok(self.status())
        }
        fn stop(&self) -> Result<RecordingStatus, RecorderError> {
            Ok(RecordingStatus::idle())
        }
    }
    let server = WebServer::bind_with_recorder(
        0,
        Arc::new(MemorySource::new()),
        Some(Arc::new(QuietRecorder)),
    )
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
    tokio::spawn(async move {
        server.serve().await.unwrap();
    });
    let response = h
        .get(
            "/api/recording/transcript?session_id=quiet",
            &h.authorised(),
        )
        .await;
    assert_eq!(response.status, 200);
    let snapshot: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(snapshot["truncated"], false);
    assert!(snapshot["speech_origin_ms"].is_null());
    assert!(snapshot["deltas"].as_array().unwrap().is_empty());
}
