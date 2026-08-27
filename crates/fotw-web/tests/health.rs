//! `GET /api/health` — what the daemon is doing, asked from outside (#101).
//!
//! The question that cost a misdiagnosis on 2026-08-25 was "is summarization
//! working, and what has it done in the last hour?", and the only way to
//! answer it was to kill the running daemon and relaunch it in a terminal.
//! This is that question as an endpoint.
//!
//! Three properties, the first two shared with every other control:
//!
//! 1. **A daemon with no health surface is indistinguishable from one with no
//!    such route** (ING-09) — the read-only preview server has no daemon
//!    behind it.
//! 2. **It needs the bearer**, like everything that is not the handoff.
//! 3. **"Never ran" is not "ran and did nothing".** That distinction is the
//!    entire issue, so it is on the wire as `null` versus a report, and a
//!    surface that collapsed the two would be the bug with a JSON hat on.

mod common;

use std::sync::Arc;

use fotw_web::{Activity, DaemonHealth, HealthReport, MemorySource, WebServer};

/// A daemon that has been up a while and done some of the work.
#[derive(Debug)]
struct FakeHealth(HealthReport);

impl DaemonHealth for FakeHealth {
    fn report(&self) -> HealthReport {
        self.0.clone()
    }
}

fn busy() -> HealthReport {
    HealthReport {
        started_at_ms: 1_700_000_000_000,
        engine: "codex-cli /opt/homebrew/bin/codex".to_owned(),
        awaiting_enrichment: 16,
        backfill: Some(Activity {
            at_ms: 1_700_000_600_000,
            summary: "3 attempted, 2 summarised, 1 not, 16 still awaiting".to_owned(),
        }),
        github: Some(Activity {
            at_ms: 1_700_000_660_000,
            summary: "nothing owed".to_owned(),
        }),
        retention: None,
        log_path: Some("/Users/x/Library/Application Support/fotw/fotwd.log".to_owned()),
    }
}

/// A server whose health surface is bound the way the daemon binds it: after
/// the listener exists, because the port and the log path are things `bind`
/// produces.
async fn rig() -> common::Harness {
    let server = WebServer::bind(0, Arc::new(MemorySource::new()))
        .await
        .expect("bind loopback");
    server.state().set_health(Arc::new(FakeHealth(busy())));

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
    h
}

#[tokio::test]
async fn a_daemon_with_no_health_surface_answers_the_bare_404() {
    let h = common::start().await;
    let res = h.get("/api/health", &h.authorised()).await;
    assert_eq!(
        res.status, 404,
        "ING-09: no route and no control look alike"
    );
    assert!(res.body.is_empty());
}

#[tokio::test]
async fn the_health_surface_needs_the_bearer_like_every_other_api_path() {
    let h = rig().await;
    assert_eq!(h.get("/api/health", &h.anonymous()).await.status, 404);
    assert_eq!(h.get("/api/health", &h.authorised()).await.status, 200);
}

#[tokio::test]
async fn the_report_answers_the_question_that_cost_the_misdiagnosis() {
    let h = rig().await;
    let res = h.get("/api/health", &h.authorised()).await;
    assert_eq!(res.status, 200);
    let body: HealthReport = serde_json::from_str(&res.body).expect("a health report");

    assert_eq!(body.engine, "codex-cli /opt/homebrew/bin/codex");
    assert_eq!(body.awaiting_enrichment, 16);
    assert_eq!(
        body.backfill.expect("a pass").summary,
        "3 attempted, 2 summarised, 1 not, 16 still awaiting"
    );
    assert!(
        body.retention.is_none(),
        "a sweeper that has not run yet reports nothing, not a zeroed report — \
         `null` and `0 evicted` are the two answers this endpoint exists to \
         keep apart"
    );
    assert!(body.log_path.is_some(), "where the rest of the story is");
}
