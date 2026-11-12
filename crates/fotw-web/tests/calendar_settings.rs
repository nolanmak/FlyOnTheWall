//! Calendar settings: authenticated, and the Composio key never comes back.
mod common;
use fotw_web::calendar::*;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct Fake {
    actions: Mutex<Vec<String>>,
    key_saved: Mutex<bool>,
    accounts: Mutex<Vec<GoogleAccount>>,
}

impl CalendarControl for Fake {
    fn read(&self) -> CalendarSettingsResponse {
        CalendarSettingsResponse {
            apple_access: "granted".into(),
            composio_key_saved: *self.key_saved.lock().unwrap(),
            composio_acknowledged: false,
            google_accounts: self.accounts.lock().unwrap().clone(),
            redirect_url: None,
            error: None,
        }
    }
    fn act(&self, action: CalendarAction) -> Result<Option<String>, String> {
        self.actions.lock().unwrap().push(format!("{action:?}"));
        match action {
            CalendarAction::SaveKey { key } if key == "bad" => {
                Err("Composio rejected that key".into())
            }
            CalendarAction::SaveKey { .. } => {
                *self.key_saved.lock().unwrap() = true;
                Ok(None)
            }
            CalendarAction::Connect { acknowledge_egress } => {
                if !acknowledge_egress {
                    return Err("tick the box first".into());
                }
                self.accounts.lock().unwrap().push(GoogleAccount {
                    id: "ca_1".into(),
                    status: AccountState::Pending,
                    added_at_ms: 1,
                });
                Ok(Some("https://connect.composio.dev/link/abc".into()))
            }
            _ => Ok(None),
        }
    }
}

async fn rig() -> (common::Harness, Arc<Fake>) {
    let h = common::start().await;
    let control = Arc::new(Fake::default());
    h.state.set_calendar(control.clone());
    (h, control)
}

const ROUTE: &str = "/api/settings/calendar";

#[tokio::test]
async fn calendar_routes_require_authentication() {
    let (h, c) = rig().await;
    for response in [
        h.get(ROUTE, &h.anonymous()).await,
        h.post(
            ROUTE,
            &h.anonymous(),
            Some(r#"{"action":"save_key","key":"k"}"#),
        )
        .await,
    ] {
        assert_eq!(response.status, 404);
        assert!(response.body.is_empty());
    }
    assert!(c.actions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn absent_on_a_server_without_the_control() {
    let h = common::start().await;
    assert_eq!(h.get(ROUTE, &h.authorised()).await.status, 404);
}

#[tokio::test]
async fn reads_the_status() {
    let (h, _) = rig().await;
    let response = h.get(ROUTE, &h.authorised()).await;
    assert_eq!(response.status, 200);
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["apple_access"], "granted");
    assert_eq!(body["composio_key_saved"], false);
}

#[tokio::test]
async fn a_saved_key_is_never_echoed_or_logged() {
    let (h, c) = rig().await;
    let response = h
        .post(
            ROUTE,
            &h.authorised(),
            Some(r#"{"action":"save_key","key":"sk-very-secret"}"#),
        )
        .await;
    assert_eq!(response.status, 200);
    assert!(!response.body.contains("sk-very-secret"));
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["composio_key_saved"], true);
    // The control's Debug of the action is what any log would print.
    let logged = c.actions.lock().unwrap().join("\n");
    assert!(!logged.contains("sk-very-secret"), "{logged}");
}

#[tokio::test]
async fn a_refused_key_reports_why() {
    let (h, _) = rig().await;
    let response = h
        .post(
            ROUTE,
            &h.authorised(),
            Some(r#"{"action":"save_key","key":"bad"}"#),
        )
        .await;
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["error"], "Composio rejected that key");
    assert_eq!(body["composio_key_saved"], false);
}

#[tokio::test]
async fn connect_returns_the_sign_in_link_and_the_pending_account() {
    let (h, _) = rig().await;
    let response = h
        .post(
            ROUTE,
            &h.authorised(),
            Some(r#"{"action":"connect","acknowledge_egress":true}"#),
        )
        .await;
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(
        body["redirect_url"],
        "https://connect.composio.dev/link/abc"
    );
    assert_eq!(body["google_accounts"][0]["status"], "pending");
}

#[tokio::test]
async fn connect_without_the_acknowledgement_is_refused_by_the_control() {
    let (h, _) = rig().await;
    let response = h
        .post(ROUTE, &h.authorised(), Some(r#"{"action":"connect"}"#))
        .await;
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["error"], "tick the box first");
    assert!(
        body.get("redirect_url")
            .is_none_or(serde_json::Value::is_null)
    );
}

#[tokio::test]
async fn malformed_actions_are_refused_without_calling_the_control() {
    let (h, c) = rig().await;
    for body in [
        r#"{}"#,
        r#"{"action":"launch_missiles"}"#,
        r#"{"action":"disconnect"}"#,
        r#"{"action":"save_key"}"#,
    ] {
        let response = h.post(ROUTE, &h.authorised(), Some(body)).await;
        assert_eq!(response.status, 404, "{body}");
    }
    assert!(c.actions.lock().unwrap().is_empty());
}

#[test]
fn an_oversized_or_blank_key_is_refused_before_the_control() {
    assert!(
        CalendarAction::SaveKey { key: "   ".into() }
            .validated()
            .is_err()
    );
    assert!(
        CalendarAction::SaveKey {
            key: "x".repeat(4096)
        }
        .validated()
        .is_err()
    );
    assert!(
        CalendarAction::Disconnect {
            id: "../../etc".into()
        }
        .validated()
        .is_err()
    );
    assert!(
        CalendarAction::Disconnect {
            id: "ca_AbC-12_x".into()
        }
        .validated()
        .is_ok()
    );
}
