//! Attendee release routes (#332): authenticated, shaped like the contract,
//! and safe when the daemon has not attached a release control.
mod common;
use fotw_web::release::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Fake {
    stored: Mutex<ReleaseSettings>,
    actions: Mutex<Vec<(String, OutboxAction)>>,
    statuses: Mutex<Vec<String>>,
}

fn item(state: &str, reasons: &[&str]) -> OutboxItem {
    OutboxItem {
        meeting_id: "m1".into(),
        event_title: "Weekly sync".into(),
        state: state.into(),
        reasons: reasons.iter().map(|r| (*r).to_owned()).collect(),
        recipient_count: 2,
        finding_count: 1,
        created_at: 1_000,
        send_at: None,
        event_start_ms: Some(1_790_000_000_000),
        event_end_ms: Some(1_790_001_800_000),
    }
}

fn detail(state: &str, reasons: &[&str]) -> OutboxDetail {
    OutboxDetail {
        item: item(state, reasons),
        recipients: vec![OutboxRecipient {
            address: "ada@example.com".into(),
            display_name: Some("Ada".into()),
        }],
        subject: "Notes: Weekly sync".into(),
        body: "Stored body".into(),
        attachment: "# Notes: Weekly sync".into(),
        findings: vec![OutboxFinding {
            category: "personal".into(),
            indices: vec![3],
            bullet: Some(("summary".into(), 0)),
            reason: "Mentions health".into(),
        }],
        segments: vec![OutboxSegment {
            index: 3,
            offset_ms: 12_000,
            timestamp: "00:00:12 | 10:00:12 AM EST".into(),
            speaker: "Ada".into(),
            text: "Hello".into(),
            retained: true,
        }],
    }
}

impl ReleaseControl for Fake {
    fn settings(&self) -> ReleaseSettingsResponse {
        ReleaseSettingsResponse {
            settings: self.stored.lock().unwrap().clone(),
            error: None,
            auto_unlocked: false,
            paused: Some("gws_unavailable".into()),
        }
    }
    fn set_settings(&self, settings: ReleaseSettings) -> BoxFuture<'_, ReleaseSettingsResponse> {
        Box::pin(async move {
            if settings.mode == ReleaseMode::Auto {
                let mut refused = self.settings();
                refused.error = Some("auto mode is locked".into());
                return refused;
            }
            *self.stored.lock().unwrap() = settings;
            self.settings()
        })
    }
    fn meeting_status(&self, meeting_id: &str) -> MeetingReleaseStatus {
        self.statuses.lock().unwrap().push(meeting_id.to_owned());
        MeetingReleaseStatus {
            state: "scheduled".into(),
            reasons: vec![],
            recipient_count: 2,
            send_at: Some(5_000),
            sent_at: None,
            event_title: Some("Weekly sync".into()),
            event_start_ms: Some(1_790_000_000_000),
            event_end_ms: None,
        }
    }
    fn outbox(&self) -> Outbox {
        Outbox {
            items: vec![item("needs_review", &["has_findings"])],
            badge: 1,
            paused: None,
        }
    }
    fn detail(&self, meeting_id: &str) -> Option<OutboxDetail> {
        (meeting_id == "m1").then(|| detail("needs_review", &["has_findings"]))
    }
    fn act(&self, meeting_id: &str, action: OutboxAction) -> Result<OutboxDetail, String> {
        self.actions
            .lock()
            .unwrap()
            .push((meeting_id.to_owned(), action.clone()));
        match action {
            OutboxAction::Cancel => Err("This release has already started sending.".into()),
            OutboxAction::WrongEvent => Ok(detail("dropped", &["wrong_event"])),
            _ => Ok(detail("scheduled", &[])),
        }
    }
}

async fn rig() -> (common::Harness, Arc<Fake>) {
    let h = common::start().await;
    let control = Arc::new(Fake::default());
    h.state.set_release(control.clone());
    (h, control)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

#[tokio::test]
async fn every_release_route_requires_authentication() {
    let (h, c) = rig().await;
    let anon = h.anonymous();
    for response in [
        h.get("/api/settings/release", &anon).await,
        h.post("/api/settings/release", &anon, Some(r#"{"enabled":true}"#))
            .await,
        h.get("/api/meetings/m1/release", &anon).await,
        h.get("/api/release/outbox", &anon).await,
        h.get("/api/release/outbox/m1", &anon).await,
        h.post(
            "/api/release/outbox/m1",
            &anon,
            Some(r#"{"action":"approve"}"#),
        )
        .await,
    ] {
        assert_eq!(response.status, 404);
        assert!(response.body.is_empty());
    }
    assert!(c.actions.lock().unwrap().is_empty());
    assert!(c.statuses.lock().unwrap().is_empty());
    assert_eq!(*c.stored.lock().unwrap(), ReleaseSettings::default());
}

#[tokio::test]
async fn settings_read_returns_the_contract_shape() {
    let (h, _) = rig().await;
    let response = h.get("/api/settings/release", &h.authorised()).await;
    assert_eq!(response.status, 200);
    let body = json(&response.body);
    assert_eq!(body["settings"]["enabled"], false);
    assert_eq!(body["settings"]["mode"], "approval");
    assert_eq!(body["settings"]["send_delay_minutes"], 15);
    assert_eq!(body["auto_unlocked"], false);
    assert_eq!(body["paused"], "gws_unavailable");
    assert!(body.get("error").is_none());
}

#[tokio::test]
async fn settings_save_stores_and_a_refusal_reports_why() {
    let (h, c) = rig().await;
    let saved = h
        .post(
            "/api/settings/release",
            &h.authorised(),
            Some(r#"{"enabled":true,"allowed_domains":["example.com"],"send_delay_minutes":0}"#),
        )
        .await;
    assert_eq!(saved.status, 200);
    let body = json(&saved.body);
    assert_eq!(body["settings"]["allowed_domains"][0], "example.com");
    assert_eq!(body["settings"]["send_delay_minutes"], 0);
    assert!(c.stored.lock().unwrap().enabled);

    let refused = h
        .post(
            "/api/settings/release",
            &h.authorised(),
            Some(r#"{"enabled":true,"mode":"auto"}"#),
        )
        .await;
    assert_eq!(refused.status, 200);
    let body = json(&refused.body);
    assert_eq!(body["error"], "auto mode is locked");
    assert_eq!(
        body["settings"]["mode"], "approval",
        "still the stored copy"
    );
}

#[tokio::test]
async fn a_malformed_settings_body_is_a_bare_404() {
    let (h, c) = rig().await;
    let response = h
        .post(
            "/api/settings/release",
            &h.authorised(),
            Some(r#"{"send_delay_minutes":"soon"}"#),
        )
        .await;
    assert_eq!(response.status, 404);
    assert!(response.body.is_empty());
    assert_eq!(*c.stored.lock().unwrap(), ReleaseSettings::default());
}

#[tokio::test]
async fn meeting_status_comes_from_the_control() {
    let (h, c) = rig().await;
    let response = h.get("/api/meetings/m1/release", &h.authorised()).await;
    assert_eq!(response.status, 200);
    let body = json(&response.body);
    assert_eq!(body["state"], "scheduled");
    assert_eq!(body["recipient_count"], 2);
    assert_eq!(body["send_at"], 5_000);
    assert!(body.get("sent_at").is_none());
    assert_eq!(*c.statuses.lock().unwrap(), vec!["m1".to_owned()]);
}

#[tokio::test]
async fn outbox_list_and_detail_match_the_contract() {
    let (h, _) = rig().await;
    let list = json(&h.get("/api/release/outbox", &h.authorised()).await.body);
    assert_eq!(list["badge"], 1);
    assert_eq!(list["items"][0]["meeting_id"], "m1");
    assert_eq!(list["items"][0]["reasons"][0], "has_findings");
    assert!(list["paused"].is_null());

    let response = h.get("/api/release/outbox/m1", &h.authorised()).await;
    assert_eq!(response.status, 200);
    let one = json(&response.body);
    assert_eq!(one["subject"], "Notes: Weekly sync");
    assert_eq!(one["recipients"][0]["address"], "ada@example.com");
    assert_eq!(one["findings"][0]["bullet"][0], "summary");
    assert_eq!(one["segments"][0]["retained"], true);

    let missing = h.get("/api/release/outbox/m2", &h.authorised()).await;
    assert_eq!(missing.status, 404);
    assert!(missing.body.is_empty());
}

#[tokio::test]
async fn actions_reach_the_control_and_a_lost_race_is_a_409_with_text() {
    let (h, c) = rig().await;
    let approve = h
        .post(
            "/api/release/outbox/m1",
            &h.authorised(),
            Some(r#"{"action":"approve"}"#),
        )
        .await;
    assert_eq!(approve.status, 200);
    assert_eq!(json(&approve.body)["item"]["state"], "scheduled");

    let edit = h
        .post(
            "/api/release/outbox/m1",
            &h.authorised(),
            Some(r#"{"action":"edit_cuts","retained":[1,3]}"#),
        )
        .await;
    assert_eq!(edit.status, 200);

    let cancel = h
        .post(
            "/api/release/outbox/m1",
            &h.authorised(),
            Some(r#"{"action":"cancel"}"#),
        )
        .await;
    assert_eq!(cancel.status, 409);
    assert_eq!(
        json(&cancel.body)["error"],
        "This release has already started sending."
    );

    let bogus = h
        .post(
            "/api/release/outbox/m1",
            &h.authorised(),
            Some(r#"{"action":"send_everything"}"#),
        )
        .await;
    assert_eq!(bogus.status, 404);

    let seen = c.actions.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            ("m1".to_owned(), OutboxAction::Approve),
            (
                "m1".to_owned(),
                OutboxAction::EditCuts {
                    retained: vec![1, 3]
                }
            ),
            ("m1".to_owned(), OutboxAction::Cancel),
        ]
    );
}

#[tokio::test]
async fn the_matched_event_is_in_the_status_and_the_outbox() {
    let (h, _) = rig().await;
    let status = json(
        &h.get("/api/meetings/m1/release", &h.authorised())
            .await
            .body,
    );
    assert_eq!(status["event_title"], "Weekly sync");
    assert_eq!(status["event_start_ms"], 1_790_000_000_000_i64);
    assert!(status["event_end_ms"].is_null());

    let list = json(&h.get("/api/release/outbox", &h.authorised()).await.body);
    assert_eq!(list["items"][0]["event_start_ms"], 1_790_000_000_000_i64);
    assert_eq!(list["items"][0]["event_end_ms"], 1_790_001_800_000_i64);
    let one = json(&h.get("/api/release/outbox/m1", &h.authorised()).await.body);
    assert_eq!(one["item"]["event_start_ms"], 1_790_000_000_000_i64);
}

#[tokio::test]
async fn wrong_event_reaches_the_control_and_returns_the_dropped_row() {
    let (h, c) = rig().await;
    let response = h
        .post(
            "/api/release/outbox/m1",
            &h.authorised(),
            Some(r#"{"action":"wrong_event"}"#),
        )
        .await;
    assert_eq!(response.status, 200);
    let body = json(&response.body);
    assert_eq!(body["item"]["state"], "dropped");
    assert_eq!(body["item"]["reasons"][0], "wrong_event");
    assert_eq!(
        *c.actions.lock().unwrap(),
        vec![("m1".to_owned(), OutboxAction::WrongEvent)]
    );
}

#[test]
fn documents_without_event_times_still_parse() {
    let status: MeetingReleaseStatus =
        serde_json::from_str(r#"{"state":"sent","reasons":[],"recipient_count":2}"#).unwrap();
    assert_eq!(status.event_title, None);
    assert_eq!((status.event_start_ms, status.event_end_ms), (None, None));
    let item: OutboxItem = serde_json::from_str(
        r#"{"meeting_id":"m1","event_title":"Weekly sync","state":"held","reasons":[],
            "recipient_count":1,"finding_count":0,"created_at":1,"send_at":null}"#,
    )
    .unwrap();
    assert_eq!((item.event_start_ms, item.event_end_ms), (None, None));
}

#[tokio::test]
async fn without_a_control_settings_are_defaults_with_an_error() {
    let h = common::start().await;
    for response in [
        h.get("/api/settings/release", &h.authorised()).await,
        h.post(
            "/api/settings/release",
            &h.authorised(),
            Some(r#"{"enabled":true}"#),
        )
        .await,
    ] {
        assert_eq!(response.status, 200);
        let body = json(&response.body);
        assert_eq!(body["error"], "attendee release is not available");
        assert_eq!(body["settings"]["enabled"], false);
        assert_eq!(body["auto_unlocked"], false);
    }
}

#[tokio::test]
async fn without_a_control_meetings_are_not_eligible_and_the_outbox_is_empty() {
    let h = common::start().await;
    let status = h.get("/api/meetings/m1/release", &h.authorised()).await;
    assert_eq!(status.status, 200);
    let body = json(&status.body);
    assert_eq!(body["state"], "not_eligible");
    assert_eq!(body["reasons"], serde_json::json!(["release_off"]));
    assert!(
        body.get("event_title").is_none(),
        "no row, no matched event"
    );
    assert!(body.get("event_start_ms").is_none());
    assert_eq!(body["recipient_count"], 0);

    let outbox = h.get("/api/release/outbox", &h.authorised()).await;
    assert_eq!(outbox.status, 200);
    let body = json(&outbox.body);
    assert_eq!(body["items"], serde_json::json!([]));
    assert_eq!(body["badge"], 0);

    assert_eq!(
        h.get("/api/release/outbox/m1", &h.authorised())
            .await
            .status,
        404
    );
    assert_eq!(
        h.post(
            "/api/release/outbox/m1",
            &h.authorised(),
            Some(r#"{"action":"approve"}"#)
        )
        .await
        .status,
        404
    );
}
