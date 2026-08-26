//! `GET/POST /api/settings/summarize` — the engine, from the product (#74).
//!
//! Before this there was no way to configure summarisation except a terminal
//! command, and no way at all for the dashboard to say whether the engine was
//! off, broken or fine. The same three properties as the GitHub pair matter
//! here, and one more:
//!
//! 1. **A daemon without the control is indistinguishable from one with no
//!    such route** — byte for byte, `Date` aside (ING-09).
//! 2. **Refusals ride in the body beside a 200.** Whether this machine has a
//!    `claude` binary is a fact about this machine.
//! 3. **Validation lives at the HTTP layer, once**, so every implementation
//!    gets settings that already passed.
//! 4. **KEY-04's disclosure is enforced by the API, not by the form.** Turning
//!    the CLI on without acknowledging that transcripts leave the machine is
//!    refused here exactly as `--i-acknowledge-egress` refuses it in the CLI.
//!    A checkbox the client can simply not render is not a control.

mod common;

use std::sync::Arc;
use std::sync::Mutex;

use fotw_web::{
    MemorySource, SummarizeControl, SummarizeError, SummarizeSettingsDoc, SummarizeStatus,
    WebServer,
};

/// A control that stores settings and reports a fixed resolution.
#[derive(Debug, Default)]
struct FakeSummarize {
    settings: Mutex<SummarizeSettingsDoc>,
    /// Set when `set_settings` should fail the way a jammed library would.
    outcome: Mutex<Option<SummarizeError>>,
}

impl SummarizeControl for FakeSummarize {
    fn settings(&self) -> SummarizeSettingsDoc {
        self.settings.lock().unwrap().clone()
    }

    fn set_settings(
        &self,
        s: SummarizeSettingsDoc,
    ) -> Result<SummarizeSettingsDoc, SummarizeError> {
        if let Some(err) = self.outcome.lock().unwrap().take() {
            return Err(err);
        }
        *self.settings.lock().unwrap() = s.clone();
        Ok(s)
    }

    fn status(&self) -> SummarizeStatus {
        let settings = self.settings();
        SummarizeStatus {
            engine: if settings.cli_enabled && settings.acknowledged_egress {
                "claude-cli".to_owned()
            } else {
                "none".to_owned()
            },
            binary_resolves: !settings.binary.is_empty(),
            configured_binary: settings.binary.clone(),
            resolved_binary: (!settings.binary.is_empty())
                .then(|| format!("/opt/homebrew/bin/{}", settings.binary)),
            api_key_present: false,
            disclosures: SummarizeStatus::all_disclosures(),
        }
    }
}

struct Rig {
    h: common::Harness,
    control: Arc<FakeSummarize>,
}

async fn rig() -> Rig {
    let control = Arc::new(FakeSummarize::default());
    let server = WebServer::bind_with_all_controls(
        0,
        Arc::new(MemorySource::new()),
        None,
        None,
        Some(Arc::clone(&control) as Arc<dyn SummarizeControl>),
    )
    .await
    .expect("bind loopback");

    let addr = server.addr();
    let state = server.state().clone();
    let h = common::Harness {
        addr,
        token: state.policy().secret().expose_hex(),
        authority: state.policy().authority().to_owned(),
        origin: state.policy().origin().to_owned(),
        state,
    };
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    Rig { h, control }
}

fn body_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("json body")
}

/// The CLI engine, turned on the way the form would turn it on.
fn acknowledged() -> String {
    r#"{"cli_enabled":true,"acknowledged_egress":true,"cli_kind":"claude","binary":"claude"}"#
        .to_owned()
}

// ------------------------------------------------------------------ ING-05

#[tokio::test]
async fn every_summarize_route_needs_the_bearer() {
    let r = rig().await;
    let anon = r.h.anonymous();

    for res in [
        r.h.get("/api/settings/summarize", &anon).await,
        r.h.post("/api/settings/summarize", &anon, Some(&acknowledged()))
            .await,
    ] {
        assert_eq!(
            res.status, 404,
            "a summarize route leaked to an anonymous caller"
        );
        assert!(res.body.is_empty());
    }
    assert!(
        !r.control.settings().cli_enabled,
        "an unauthenticated request reached the control"
    );
}

// ------------------------------------------------------------------ ING-09

/// A build with no engine control and a path that does not exist are one
/// response, byte for byte.
#[tokio::test]
async fn a_server_without_the_control_answers_the_same_404_as_an_unknown_path() {
    let h = common::start().await;

    let absent = h.get("/api/settings/summarize", &h.authorised()).await;
    let unknown = h.get("/api/no-such-path", &h.authorised()).await;
    assert_eq!(absent.status, 404);
    assert_eq!(
        absent.bytes_without_date(),
        unknown.bytes_without_date(),
        "ING-09: the control's absence must not be observable"
    );

    let post = h
        .post(
            "/api/settings/summarize",
            &h.authorised(),
            Some(&acknowledged()),
        )
        .await;
    assert_eq!(post.status, 404);
    assert_eq!(post.bytes_without_date(), unknown.bytes_without_date());
}

/// A body axum cannot parse is refused exactly as a wrong token is: axum's own
/// rejection text would describe the endpoint to whoever is probing it.
#[tokio::test]
async fn a_malformed_body_is_the_same_404_as_a_wrong_token() {
    let r = rig().await;
    let res =
        r.h.post("/api/settings/summarize", &r.h.authorised(), Some("{oops"))
            .await;
    assert_eq!(res.status, 404);
    assert!(res.body.is_empty());
}

#[tokio::test]
async fn the_wrong_method_is_a_bare_404_with_no_allow_header() {
    let r = rig().await;
    let ok = r.h.get("/api/settings/summarize", &r.h.authorised()).await;
    assert_eq!(ok.status, 200, "sanity: the right method works");

    // `DELETE` is routed nowhere, so the method fallback answers.
    let wrong =
        r.h.send(&common::build(
            "DELETE",
            "/api/settings/summarize",
            &r.h.authorised(),
            None,
        ))
        .await;
    assert_eq!(wrong.status, 404);
    assert!(
        wrong.header("allow").is_none(),
        "an Allow header names the method, which names the path"
    );
}

// ----------------------------------------------------------------- settings

#[tokio::test]
async fn settings_round_trip_through_the_api() {
    let r = rig().await;

    let saved =
        r.h.post(
            "/api/settings/summarize",
            &r.h.authorised(),
            Some(&acknowledged()),
        )
        .await;
    assert_eq!(saved.status, 200);
    let saved = body_json(&saved.body);
    assert!(saved["error"].is_null(), "valid settings must save cleanly");
    assert_eq!(saved["settings"]["cli_enabled"], true);
    assert_eq!(saved["settings"]["cli_kind"], "claude");

    let read = r.h.get("/api/settings/summarize", &r.h.authorised()).await;
    assert_eq!(read.status, 200);
    let read = body_json(&read.body);
    assert_eq!(read["settings"]["binary"], "claude");
    assert_eq!(read["settings"]["acknowledged_egress"], true);
}

/// KEY-04, in its API shape. `fotwd engine claude-cli` refuses without
/// `--i-acknowledge-egress`; this is the same refusal for a caller that has no
/// terminal. It answers **beside a 200**, and it does not change what is
/// stored — a refused save that half-applied is worse than one that failed.
#[tokio::test]
async fn enabling_the_cli_without_the_acknowledgement_is_refused_beside_a_200() {
    let r = rig().await;
    let body =
        r#"{"cli_enabled":true,"acknowledged_egress":false,"cli_kind":"claude","binary":"claude"}"#;

    let res =
        r.h.post("/api/settings/summarize", &r.h.authorised(), Some(body))
            .await;

    assert_eq!(res.status, 200, "a refusal is not a status code (ING-09)");
    let v = body_json(&res.body);
    assert_eq!(
        v["error"], "disclosure_required",
        "the code the UI branches on must be stable"
    );
    assert_eq!(
        v["settings"]["cli_enabled"], false,
        "the refusal must answer with what is still stored"
    );
    assert!(
        !r.control.settings().cli_enabled,
        "a refused save reached the control anyway"
    );
}

/// Turning the engine *off* needs no acknowledgement — nothing is leaving.
#[tokio::test]
async fn switching_the_engine_off_needs_no_acknowledgement() {
    let r = rig().await;
    r.h.post(
        "/api/settings/summarize",
        &r.h.authorised(),
        Some(&acknowledged()),
    )
    .await;

    let res =
        r.h.post(
            "/api/settings/summarize",
            &r.h.authorised(),
            Some(r#"{"cli_enabled":false,"acknowledged_egress":false,"binary":""}"#),
        )
        .await;
    let v = body_json(&res.body);
    assert!(v["error"].is_null(), "off is always allowed: {}", res.body);
    assert_eq!(v["settings"]["cli_enabled"], false);
}

/// A store that refused the write is a fact about this machine, so it rides
/// in the body like every other one — and the response still says what is
/// stored, which after a failed write is the old value.
#[tokio::test]
async fn a_store_failure_rides_in_the_body_beside_a_200() {
    let r = rig().await;
    *r.control.outcome.lock().unwrap() = Some(SummarizeError::Failed("database is locked".into()));

    let res =
        r.h.post(
            "/api/settings/summarize",
            &r.h.authorised(),
            Some(&acknowledged()),
        )
        .await;

    assert_eq!(res.status, 200);
    let v = body_json(&res.body);
    assert_eq!(v["error"], "database is locked");
    assert_eq!(v["settings"]["cli_enabled"], false);
}

// ------------------------------------------------------------------- status

/// The diagnostic that cannot lie: whatever the settings row says, this is
/// what *the daemon* resolves, and it names both when they differ.
#[tokio::test]
async fn the_status_reports_what_the_daemon_resolves_not_what_was_configured() {
    let r = rig().await;
    r.h.post(
        "/api/settings/summarize",
        &r.h.authorised(),
        Some(&acknowledged()),
    )
    .await;

    let v = body_json(
        &r.h.get("/api/settings/summarize", &r.h.authorised())
            .await
            .body,
    );
    assert_eq!(v["status"]["engine"], "claude-cli");
    assert_eq!(v["status"]["binary_resolves"], true);
    assert_eq!(v["status"]["configured_binary"], "claude");
    assert_eq!(
        v["status"]["resolved_binary"], "/opt/homebrew/bin/claude",
        "a status that prints only the configured name describes a binary the \
         daemon is not running"
    );
}

/// KEY-04's words reach the client, so the checkbox is beside what it is
/// acknowledging rather than beside a link to it.
///
/// **Every** engine's words, on every response, not just the stored engine's:
/// the form has a picker, and a user switching it to codex while still reading
/// Anthropic's "not used for training by default" would be acknowledging the
/// wrong facts. Collecting the wrong acknowledgement is the one thing a
/// disclosure must not do.
#[tokio::test]
async fn the_disclosure_names_the_host_and_the_training_default() {
    let r = rig().await;
    let body = body_json(
        &r.h.get("/api/settings/summarize", &r.h.authorised())
            .await
            .body,
    );
    let all = &body["status"]["disclosures"];

    let claude = all["claude"].to_string();
    assert!(claude.contains("anthropic.com"), "name the host: {claude}");
    assert!(
        claude.contains("not used for training by default"),
        "name the retention default: {claude}"
    );

    let codex = all["codex"].to_string();
    assert!(codex.contains("openai.com"), "name the host: {codex}");
    assert!(
        codex.contains("train OpenAI"),
        "a consumer ChatGPT subscription trains by default, and saying \
         otherwise by omission is the failure this disclosure exists to \
         prevent: {codex}"
    );
}

// ------------------------------------------------------------------ KEY-01

/// Keys live in the OS keychain and never cross this API. The status carries
/// *presence*, which is what a settings form needs to render, and nothing that
/// could be a key.
///
/// Pinned as an exhaustive field list rather than a substring sweep: a field
/// named `api_key_present` contains the word "key" and is fine, while a field
/// named `binary` could one day be joined by one that is not. The list is what
/// makes a future addition a decision someone has to make on purpose.
#[tokio::test]
async fn no_secret_field_exists_on_this_api() {
    let r = rig().await;
    r.h.post(
        "/api/settings/summarize",
        &r.h.authorised(),
        Some(&acknowledged()),
    )
    .await;

    let res = r.h.get("/api/settings/summarize", &r.h.authorised()).await;
    let v = body_json(&res.body);

    let fields = |node: &serde_json::Value| {
        let mut names: Vec<String> = node
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        fields(&v["settings"]),
        ["acknowledged_egress", "binary", "cli_enabled", "cli_kind"],
        "a field appeared on the settings document"
    );
    assert_eq!(
        fields(&v["status"]),
        [
            "api_key_present",
            "binary_resolves",
            "configured_binary",
            "disclosures",
            "engine",
            "resolved_binary",
        ],
        "a field appeared on the status document"
    );

    assert!(
        v["status"]["api_key_present"].is_boolean(),
        "presence is the only thing this API may say about a key, and a \
         boolean is the only shape that cannot accidentally carry one"
    );
    assert!(
        !res.body.contains("sk-ant") && !res.body.contains("sk-proj"),
        "something key-shaped is in the body: {}",
        res.body
    );
}
