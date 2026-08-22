//! `GET/POST /api/settings/github`, `POST /api/meetings/{id}/github-push`.
//!
//! The GitHub export target (issue #63). Three properties matter more than
//! the happy path:
//!
//! 1. **A daemon without the control is indistinguishable from one with no
//!    such route.** The read-only preview server exists, and a scanning page
//!    must not learn that this build can talk to GitHub.
//! 2. **Failures ride in the body beside a 200, never in the status code.**
//!    "gh is not installed" is a fact about this machine, and ING-09 withholds
//!    facts from anything that cannot present the bearer.
//! 3. **Settings are validated at the HTTP layer, once.** The trait
//!    implementation receives only settings that already passed
//!    [`GithubSettings::normalized`], so a rule enforced here is enforced for
//!    every implementation.

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use fotw_web::{
    GithubError, GithubExport, GithubMode, GithubReceipt, GithubSettings, MemorySource, WebServer,
};

/// A control that stores settings and answers pushes without running anything.
#[derive(Debug, Default)]
struct FakeGithub {
    settings: Mutex<GithubSettings>,
    pushes: AtomicU64,
    /// The error the next push should fail with, if any.
    outcome: Mutex<Option<GithubError>>,
}

impl GithubExport for FakeGithub {
    fn settings(&self) -> GithubSettings {
        self.settings.lock().unwrap().clone()
    }

    fn set_settings(&self, s: GithubSettings) -> Result<GithubSettings, GithubError> {
        *self.settings.lock().unwrap() = s.clone();
        Ok(s)
    }

    fn push(&self, meeting_id: &str) -> Result<GithubReceipt, GithubError> {
        if let Some(err) = self.outcome.lock().unwrap().take() {
            return Err(err);
        }
        self.pushes.fetch_add(1, Ordering::Relaxed);
        Ok(GithubReceipt {
            repo: "octocat/notes".to_owned(),
            path: format!("meetings/2026-08-21-standup-{meeting_id}.md"),
            commit: "f00dcafe".to_owned(),
            pushed_at_ms: 1_787_000_000_000,
        })
    }
}

struct Rig {
    h: common::Harness,
    github: Arc<FakeGithub>,
}

async fn rig() -> Rig {
    let github = Arc::new(FakeGithub::default());
    let server = WebServer::bind_with_controls(
        0,
        Arc::new(MemorySource::new()),
        None,
        Some(Arc::clone(&github) as Arc<dyn GithubExport>),
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
    Rig { h, github }
}

fn body_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("json body")
}

/// A settings document the validator accepts.
fn good_settings() -> String {
    r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"manual"}"#
        .to_owned()
}

// ------------------------------------------------------------------ ING-05

/// All three need the bearer. None of them may join `bearer_exempt`, and this
/// is the test that fails if someone adds them.
#[tokio::test]
async fn every_github_route_needs_the_bearer() {
    let r = rig().await;
    let anon = r.h.anonymous();

    let responses = vec![
        r.h.get("/api/settings/github", &anon).await,
        r.h.post("/api/settings/github", &anon, Some(&good_settings()))
            .await,
        r.h.post("/api/meetings/m1/github-push", &anon, None).await,
    ];
    for res in responses {
        assert_eq!(
            res.status, 404,
            "a github route leaked to an anonymous caller"
        );
        assert!(res.body.is_empty());
    }
    assert_eq!(
        r.github.pushes.load(Ordering::Relaxed),
        0,
        "an unauthenticated request reached the control"
    );
}

// ------------------------------------------------------------------ ING-09

/// A daemon that cannot export and a path that does not exist are one
/// response, byte for byte.
#[tokio::test]
async fn a_server_without_the_control_answers_the_same_404_as_an_unknown_path() {
    let h = common::start().await;

    let absent = h.get("/api/settings/github", &h.authorised()).await;
    let unknown = h.get("/api/no-such-path", &h.authorised()).await;
    assert_eq!(absent.status, 404);
    assert_eq!(
        absent.bytes_without_date(),
        unknown.bytes_without_date(),
        "ING-09: the control's absence must not be observable"
    );

    let push = h
        .post("/api/meetings/m1/github-push", &h.authorised(), None)
        .await;
    assert_eq!(push.status, 404);
    assert_eq!(push.bytes_without_date(), unknown.bytes_without_date());
}

/// The wrong method on a github route is the bare 404, with no `Allow` header
/// to confirm the path exists.
#[tokio::test]
async fn the_wrong_method_is_a_bare_404_with_no_allow_header() {
    let r = rig().await;
    let res =
        r.h.post("/api/meetings/m1/github-push", &r.h.authorised(), None)
            .await;
    assert_eq!(res.status, 200, "sanity: the right method works");

    let wrong =
        r.h.get("/api/meetings/m1/github-push", &r.h.authorised())
            .await;
    assert_eq!(wrong.status, 404);
    assert!(
        wrong.header("allow").is_none(),
        "an Allow header names the method"
    );
}

// ----------------------------------------------------------------- settings

#[tokio::test]
async fn settings_round_trip_through_the_api() {
    let r = rig().await;

    let saved =
        r.h.post(
            "/api/settings/github",
            &r.h.authorised(),
            Some(&good_settings()),
        )
        .await;
    assert_eq!(saved.status, 200);
    let saved = body_json(&saved.body);
    assert!(saved["error"].is_null(), "valid settings must save cleanly");
    assert_eq!(saved["settings"]["repo"], "octocat/notes");

    let read = r.h.get("/api/settings/github", &r.h.authorised()).await;
    assert_eq!(read.status, 200);
    let read = body_json(&read.body);
    assert_eq!(read["settings"]["repo"], "octocat/notes");
    assert_eq!(read["settings"]["enabled"], true);
    assert_eq!(read["settings"]["mode"], "manual");
}

/// The path prefix is normalized on the way in — the UI should read back the
/// spelling the pusher will actually use.
#[tokio::test]
async fn a_path_prefix_is_normalized_before_it_is_stored() {
    let r = rig().await;
    let body = r#"{"enabled":true,"repo":"octocat/notes","branch":"main","path_prefix":"/notes/meetings","mode":"auto"}"#;
    let res =
        r.h.post("/api/settings/github", &r.h.authorised(), Some(body))
            .await;
    let v = body_json(&res.body);
    assert!(v["error"].is_null());
    assert_eq!(
        v["settings"]["path_prefix"], "notes/meetings/",
        "a prefix is stored without a leading slash and with a trailing one"
    );
}

/// Bad values are refused in the body, and the stored settings do not change.
#[tokio::test]
async fn invalid_settings_are_refused_in_the_body_not_the_status() {
    let r = rig().await;
    for bad in [
        // Enabled with no repo to push to.
        r#"{"enabled":true,"repo":"","branch":"","path_prefix":"meetings/","mode":"manual"}"#,
        // Not owner/name.
        r#"{"enabled":true,"repo":"octocat","branch":"","path_prefix":"meetings/","mode":"manual"}"#,
        r#"{"enabled":true,"repo":"a/b/c","branch":"","path_prefix":"meetings/","mode":"manual"}"#,
        // A prefix that escapes the repo.
        r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"../secrets/","mode":"manual"}"#,
    ] {
        let res =
            r.h.post("/api/settings/github", &r.h.authorised(), Some(bad))
                .await;
        assert_eq!(res.status, 200, "a refusal is not a status code (ING-09)");
        let v = body_json(&res.body);
        let error = v["error"].as_str().unwrap_or_default().to_owned();
        assert!(
            error.starts_with("invalid_settings"),
            "expected invalid_settings, got {error:?} for {bad}"
        );
    }

    let read = r.h.get("/api/settings/github", &r.h.authorised()).await;
    let v = body_json(&read.body);
    assert_eq!(
        v["settings"]["enabled"], false,
        "a refused save must leave the stored settings alone"
    );
}

/// A body that is not JSON at all gets the same bare 404 a wrong token gets —
/// axum's own rejection would describe the endpoint to whoever is probing it.
#[tokio::test]
async fn a_malformed_settings_body_is_a_bare_404() {
    let r = rig().await;
    let res =
        r.h.post("/api/settings/github", &r.h.authorised(), Some("not json"))
            .await;
    assert_eq!(res.status, 404);
    assert!(res.body.is_empty());
}

// -------------------------------------------------------------------- push

#[tokio::test]
async fn a_push_returns_the_receipt() {
    let r = rig().await;
    let res =
        r.h.post("/api/meetings/m1/github-push", &r.h.authorised(), None)
            .await;
    assert_eq!(res.status, 200);
    let v = body_json(&res.body);
    assert!(v["error"].is_null());
    assert_eq!(v["receipt"]["repo"], "octocat/notes");
    assert_eq!(v["receipt"]["commit"], "f00dcafe");
    assert_eq!(r.github.pushes.load(Ordering::Relaxed), 1);
}

/// A meeting that is not there and a request that was not allowed look the
/// same from outside — exactly as `GET /api/meetings/{id}` answers.
#[tokio::test]
async fn a_push_for_a_meeting_that_does_not_exist_is_a_bare_404() {
    let r = rig().await;
    *r.github.outcome.lock().unwrap() = Some(GithubError::NoSuchMeeting);
    let res =
        r.h.post("/api/meetings/gone/github-push", &r.h.authorised(), None)
            .await;
    assert_eq!(res.status, 404);
    assert!(res.body.is_empty());
}

/// Machine states — gh missing, not authenticated, repo gone — are data for
/// the UI, not HTTP failures.
#[tokio::test]
async fn a_push_failure_rides_in_the_body() {
    let r = rig().await;
    for (err, code) in [
        (GithubError::Disabled, "github_export_disabled"),
        (GithubError::GhMissing, "gh_missing"),
        (GithubError::NotAuthenticated, "gh_not_authenticated"),
        (GithubError::RepoNotFound, "repo_not_found"),
        (GithubError::Failed("HTTP 409".to_owned()), "HTTP 409"),
    ] {
        *r.github.outcome.lock().unwrap() = Some(err);
        let res =
            r.h.post("/api/meetings/m1/github-push", &r.h.authorised(), None)
                .await;
        assert_eq!(res.status, 200);
        let v = body_json(&res.body);
        assert_eq!(v["error"].as_str().unwrap_or_default(), code);
        assert!(v["receipt"].is_null());
    }
}

// ------------------------------------------------------------- wire format

/// The mode's wire spelling is part of the UI contract, as the recorder's
/// state is.
#[test]
fn the_mode_spelling_is_pinned() {
    assert_eq!(
        serde_json::to_string(&GithubMode::Manual).unwrap(),
        r#""manual""#
    );
    assert_eq!(
        serde_json::to_string(&GithubMode::Auto).unwrap(),
        r#""auto""#
    );
}

/// Defaults are what a fresh library gets: disabled, `meetings/`, manual.
#[test]
fn the_default_settings_are_off_and_manual() {
    let s = GithubSettings::default();
    assert!(!s.enabled);
    assert_eq!(s.repo, "");
    assert_eq!(s.branch, "");
    assert_eq!(s.path_prefix, "meetings/");
    assert_eq!(s.mode, GithubMode::Manual);
    assert_eq!(s.auto_since_ms, None);
}
