//! The JSON endpoints.
//!
//! Three rules run through all of them:
//!
//! * **Nothing here re-checks the host, the origin or the bearer.**
//!   [`crate::ingress::guard`] has already refused anything that fails, before
//!   routing. A handler that also checked would be a handler someone could
//!   forget to write.
//! * **Every failure a caller could use to learn something is the same bare
//!   404** — a meeting that does not exist, an asset that does not exist, a
//!   bad token. ING-09.
//! * **Every store call goes through [`tokio::task::spawn_blocking`].**
//!   `rusqlite` blocks. A two-hour transcript read on a runtime worker stalls
//!   every other connection on that worker, including the live delta stream of
//!   a meeting in progress.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use crate::assets::apply_security_headers;
use crate::github::{GithubError, GithubReceipt, GithubSettings};
use crate::ingress::not_found;
use crate::query::query_param;
use crate::recorder::{RecorderError, RecordingStatus};
use crate::source::{Hit, MeetingDetail, MeetingRow, SourceError};
use crate::state::AppState;
use crate::summarize::{SummarizeSettingsDoc, SummarizeStatus};
use crate::tokens::WS_TICKET_TTL;

/// The most meetings one request may ask for.
///
/// A cap rather than a suggestion: `?limit=1000000` on a library of ten
/// thousand meetings is a self-inflicted denial of service, and the UI
/// virtualises the list anyway (§5.5).
const MAX_LIMIT: u32 = 200;
/// What the UI gets when it does not say.
const DEFAULT_LIMIT: u32 = 50;

/// `GET /api/meetings?limit=&offset=`
#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingsResponse {
    /// Most recent first.
    pub meetings: Vec<MeetingRow>,
}

/// `GET /api/search?q=`
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    /// Best match first.
    pub hits: Vec<Hit>,
}

/// `POST /api/ws-ticket`
#[derive(Debug, Serialize, Deserialize)]
pub struct TicketResponse {
    /// Single-use, for one `GET /api/stream`.
    pub ticket: String,
    /// How long the ticket is good for, so the client does not have to
    /// hardcode ING-07's ten seconds.
    pub expires_in_ms: u64,
}

/// `POST /api/handoff`
#[derive(Debug, Serialize, Deserialize)]
pub struct HandoffRequest {
    /// The `?t=` value from the launch URL.
    pub token: String,
}

/// The reply to a redeemed handoff: the bearer token for the rest of the
/// session.
#[derive(Debug, Serialize, Deserialize)]
pub struct HandoffResponse {
    /// Goes in `Authorization: Bearer`, and into `localStorage` — never a
    /// cookie (ING-08): origin-keyed, attached only by the app's own code,
    /// zero ambient credentials.
    pub token: String,
}

/// `GET /api/recording/status`, `POST /api/recording/{start,stop}`.
///
/// The status is flattened in rather than nested so the UI reads
/// `body.state` for every one of the three endpoints and never has to know
/// which it called.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecordingResponse {
    /// What the recorder is doing.
    #[serde(flatten)]
    pub status: RecordingStatus,
    /// Why the request did not do what was asked, when it did not.
    ///
    /// Carried beside a 200 rather than as a status code: see the module note
    /// on `recorder.rs` about ING-09 and presence oracles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The value `?ack=` must carry for a start to be honoured.
///
/// Spelled out rather than a bare boolean so that a stray `?ack=1`, an empty
/// `?ack=`, or a form default cannot start a recording by accident. It is the
/// UI's tick box, and CON-01's "no silent auto-record", on the wire.
const CONSENT_ACK: &str = "all-party";

/// `GET /api/health`
///
/// Read-only, and the one endpoint whose whole value is that it can be asked
/// of a *running* daemon (#101). `spawn_blocking` because the report counts
/// the enrichment queue, and that is a SQLite read.
pub async fn health(State(state): State<AppState>) -> Response {
    let Some(health) = state.health() else {
        return not_found();
    };
    let Ok(report) = tokio::task::spawn_blocking(move || health.report()).await else {
        return not_found();
    };
    json(&state, &report)
}

/// `GET /api/recording/status`
pub async fn recording_status(State(state): State<AppState>) -> Response {
    let Some(recorder) = state.recorder() else {
        return not_found();
    };
    let status = recorder.status();
    json(
        &state,
        &RecordingResponse {
            status,
            error: None,
        },
    )
}

/// `POST /api/recording/start`
///
/// Requires `?ack=all-party`. The acknowledgement is read with
/// [`query_param`] rather than from a JSON body on purpose: an extractor that
/// can reject produces a rejection body, and a rejection body that differs
/// from a 404 is the oracle ING-09 forbids. `ws_ticket` takes no body for the
/// same reason.
pub async fn recording_start(State(state): State<AppState>, uri: Uri) -> Response {
    let Some(recorder) = state.recorder() else {
        return not_found();
    };

    if query_param(uri.query().unwrap_or_default(), "ack").as_deref() != Some(CONSENT_ACK) {
        return json(
            &state,
            &RecordingResponse {
                status: recorder.status(),
                error: Some("consent_required".to_owned()),
            },
        );
    }

    // Opening a tap blocks. On a runtime worker that would stall the live
    // delta stream of the meeting being started, which is the one thing the
    // user is watching at that moment.
    let result = tokio::task::spawn_blocking(move || {
        let outcome = recorder.start();
        (outcome, recorder.status())
    })
    .await;

    let Ok((outcome, status)) = result else {
        return server_error();
    };

    let error = match outcome {
        Ok(_) => None,
        // Not surfaced as a failure: a double-clicked button produces it, and
        // the honest answer is the state, which is already "recording".
        Err(RecorderError::AlreadyRecording) => Some("already_recording".to_owned()),
        Err(e) => Some(e.to_string()),
    };
    json(&state, &RecordingResponse { status, error })
}

/// `POST /api/recording/stop`
pub async fn recording_stop(State(state): State<AppState>) -> Response {
    let Some(recorder) = state.recorder() else {
        return not_found();
    };

    let result = tokio::task::spawn_blocking(move || {
        let outcome = recorder.stop();
        (outcome, recorder.status())
    })
    .await;

    let Ok((outcome, status)) = result else {
        return server_error();
    };

    let error = match outcome {
        // Stopping something already stopped is what a reloaded tab does. The
        // state it asked for is the state it got.
        Ok(_) | Err(RecorderError::NotRecording) => None,
        Err(e) => Some(e.to_string()),
    };
    json(&state, &RecordingResponse { status, error })
}

/// `GET /api/settings/github`, `POST /api/settings/github`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GithubSettingsResponse {
    /// What is stored — after a refused save, what is *still* stored.
    pub settings: GithubSettings,
    /// Why a save was refused, when it was. Beside a 200: see `recorder.rs`
    /// on ING-09 and presence oracles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `POST /api/launch-url`.
#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchUrlResponse {
    /// A fresh one-time login URL, `http://127.0.0.1:{port}/?t={handoff}`.
    pub url: String,
}

/// `GET /api/settings/github/repos`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GithubReposResponse {
    /// `owner/name`, most recently active first. Empty when `error` says why.
    pub repos: Vec<String>,
    /// Why the listing failed, when it did — the same stable codes a push
    /// answers with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `POST /api/meetings/{id}/github-push`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GithubPushResponse {
    /// Where the transcript landed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<GithubReceipt>,
    /// Why it did not, when it did not — the stable codes in
    /// [`GithubError`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET`/`POST /api/settings/summarize`.
///
/// The status rides along with the settings on both verbs, so the form can
/// render "this is what the daemon actually resolves" without a second round
/// trip — and so a save immediately shows whether the binary just chosen is
/// one this machine can find (#74).
#[derive(Debug, Serialize, Deserialize)]
pub struct SummarizeSettingsResponse {
    /// What is stored — after a refused save, what is *still* stored.
    pub settings: SummarizeSettingsDoc,
    /// What the daemon would resolve right now.
    pub status: SummarizeStatus,
    /// Why a save was refused, when it was: `disclosure_required` (KEY-04) or
    /// `invalid_settings: <why>`. Beside a 200, per ING-09.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET /api/settings/summarize`
pub async fn summarize_settings(State(state): State<AppState>) -> Response {
    let Some(control) = state.summarize() else {
        return not_found();
    };
    // The library and the keychain both block.
    let result = tokio::task::spawn_blocking(move || (control.settings(), control.status())).await;
    let Ok((settings, status)) = result else {
        return server_error();
    };
    json(
        &state,
        &SummarizeSettingsResponse {
            settings,
            status,
            error: None,
        },
    )
}

/// `POST /api/settings/summarize`
///
/// Validation runs here, once, so the control behind the trait only ever
/// stores what [`SummarizeSettingsDoc::normalized`] accepted — including
/// KEY-04's refusal to enable the CLI without the egress acknowledgement.
pub async fn summarize_set_settings(
    State(state): State<AppState>,
    body: Result<axum::Json<SummarizeSettingsDoc>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(control) = state.summarize() else {
        return not_found();
    };
    // A malformed body is refused exactly as a wrong token is — axum's own
    // rejection text describes the endpoint to whoever is probing it.
    let Ok(axum::Json(submitted)) = body else {
        return not_found();
    };

    let result = tokio::task::spawn_blocking(move || {
        let outcome = match submitted.normalized() {
            Ok(valid) => match control.set_settings(valid) {
                Ok(stored) => (stored, None),
                Err(e) => (control.settings(), Some(e.to_string())),
            },
            // The refusal answers with what is still stored, so the UI never
            // has to guess whether a rejected save changed anything.
            Err(e) => (control.settings(), Some(e.to_string())),
        };
        // Read the status *after* the write, so a save that fixed the binary
        // reports as fixed in the same response.
        (outcome.0, control.status(), outcome.1)
    })
    .await;

    let Ok((settings, status, error)) = result else {
        return server_error();
    };
    json(
        &state,
        &SummarizeSettingsResponse {
            settings,
            status,
            error,
        },
    )
}

/// `GET /api/settings/github`
pub async fn github_settings(State(state): State<AppState>) -> Response {
    let Some(github) = state.github() else {
        return not_found();
    };
    // Reading the settings is a library read; rusqlite blocks.
    let result = tokio::task::spawn_blocking(move || github.settings()).await;
    let Ok(settings) = result else {
        return server_error();
    };
    json(
        &state,
        &GithubSettingsResponse {
            settings,
            error: None,
        },
    )
}

/// `GET /api/settings/github/repos`
///
/// The picker behind the settings form. A subprocess call, so it runs on the
/// blocking pool like a push does.
pub async fn github_repos(State(state): State<AppState>) -> Response {
    let Some(github) = state.github() else {
        return not_found();
    };
    let result = tokio::task::spawn_blocking(move || github.repos()).await;
    let Ok(outcome) = result else {
        return server_error();
    };
    let (repos, error) = match outcome {
        Ok(repos) => (repos, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    json(&state, &GithubReposResponse { repos, error })
}

/// `POST /api/settings/github`
///
/// The body is the settings document. Validation runs here, once, so the
/// control behind the trait only ever stores what
/// [`GithubSettings::normalized`] accepted.
pub async fn github_set_settings(
    State(state): State<AppState>,
    body: Result<axum::Json<GithubSettings>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(github) = state.github() else {
        return not_found();
    };
    // A malformed body is refused exactly as a wrong token is — axum's own
    // rejection text describes the endpoint to whoever is probing it.
    let Ok(axum::Json(submitted)) = body else {
        return not_found();
    };

    let result = tokio::task::spawn_blocking(move || match submitted.normalized() {
        Ok(valid) => match github.set_settings(valid) {
            Ok(stored) => (stored, None),
            Err(e) => (github.settings(), Some(e.to_string())),
        },
        // The refusal answers with what is still stored, so the UI never has
        // to guess whether a rejected save changed anything.
        Err(why) => (
            github.settings(),
            Some(GithubError::Invalid(why).to_string()),
        ),
    })
    .await;

    let Ok((settings, error)) = result else {
        return server_error();
    };
    json(&state, &GithubSettingsResponse { settings, error })
}

/// `POST /api/meetings/{id}/github-push`
pub async fn github_push(
    State(state): State<AppState>,
    id: Result<Path<String>, PathErr>,
) -> Response {
    let Some(github) = state.github() else {
        return not_found();
    };
    let Ok(Path(id)) = id else {
        return not_found();
    };

    // A push is a subprocess making network calls; minutes of transcript are
    // read from the library first. Neither belongs on a runtime worker. The
    // bundle sync rides in the same blocking task: best-effort, because the
    // meeting file has already landed and the index is derived from it.
    let result = tokio::task::spawn_blocking(move || {
        let outcome = github.push(&id);
        if outcome.is_ok() {
            let _ = github.sync_bundle();
        }
        outcome
    })
    .await;
    let Ok(outcome) = result else {
        return server_error();
    };

    match outcome {
        Ok(receipt) => json(
            &state,
            &GithubPushResponse {
                receipt: Some(receipt),
                error: None,
            },
        ),
        // The same bare 404 `GET /api/meetings/{id}` answers: a guessed id
        // must not be confirmable through this route either.
        Err(GithubError::NoSuchMeeting) => not_found(),
        Err(e) => json(
            &state,
            &GithubPushResponse {
                receipt: None,
                error: Some(e.to_string()),
            },
        ),
    }
}

/// `GET /api/meetings`
pub async fn list_meetings(State(state): State<AppState>, uri: Uri) -> Response {
    let query = uri.query().unwrap_or_default();
    let limit = number(query, "limit")
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);
    let offset = number(query, "offset").unwrap_or(0);

    match blocking(state.source(), move |s| s.list(limit, offset)).await {
        Ok(meetings) => json(&state, &MeetingsResponse { meetings }),
        Err(_) => server_error(),
    }
}

/// `GET /api/meetings/{id}`
pub async fn get_meeting(
    State(state): State<AppState>,
    id: Result<Path<String>, PathErr>,
) -> Response {
    let Ok(Path(id)) = id else {
        return not_found();
    };
    match blocking(state.source(), move |s| s.detail(&id)).await {
        // A meeting that is not there and a request that was not allowed look
        // the same from outside. That is the point: otherwise a page that
        // guessed a meeting id could confirm the guess without a token.
        Ok(Some(detail)) => json::<MeetingDetail>(&state, &detail),
        Ok(None) => not_found(),
        Err(_) => server_error(),
    }
}

/// `GET /api/search?q=`
pub async fn search(State(state): State<AppState>, uri: Uri) -> Response {
    let query = uri.query().unwrap_or_default();
    let q = query_param(query, "q").unwrap_or_default();
    let limit = number(query, "limit")
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);
    if q.trim().is_empty() {
        return json(&state, &SearchResponse { hits: Vec::new() });
    }

    match blocking(state.source(), move |s| s.search(&q, limit)).await {
        Ok(hits) => json(&state, &SearchResponse { hits }),
        // "C++", a lone quote, or a string of punctuation is a query with no
        // searchable token in it. `fotw_store` refuses those rather than
        // guessing, and to a search box the honest rendering of that refusal
        // is an empty result list, not an error dialog.
        Err(SourceError::BadQuery(_)) => json(&state, &SearchResponse { hits: Vec::new() }),
        Err(SourceError::Backend(_)) => server_error(),
    }
}

/// `POST /api/ws-ticket` — ING-07.
///
/// Requires the bearer token, which is the entire mechanism: the ticket is a
/// credential a page can only obtain by already holding a credential it cannot
/// obtain.
pub async fn ws_ticket(State(state): State<AppState>) -> Response {
    let ticket = state.tickets().mint();
    json(
        &state,
        &TicketResponse {
            ticket,
            expires_in_ms: u64::try_from(WS_TICKET_TTL.as_millis()).unwrap_or(u64::MAX),
        },
    )
}

/// `POST /api/launch-url`
///
/// The re-entry path: the launch URL the daemon opened at startup is worth
/// one redemption, so a closed tab used to mean restarting the daemon. A
/// caller holding the bearer — the CLI reading the 0600 state file, or an
/// authorized tab — can mint another. The reply carries a handoff token,
/// never the bearer itself, so the URL is still safe in `open(1)`'s argv and
/// browser history, and same-user local processes are outside the threat
/// model anyway (§10.1).
pub async fn launch_url(State(state): State<AppState>) -> Response {
    json(
        &state,
        &LaunchUrlResponse {
            url: state.launch_url(),
        },
    )
}

/// `POST /api/handoff` — ING-10.
///
/// The one endpoint that hands out the bearer token, in exchange for the
/// one-time token from the launch URL. Reachable without a bearer by
/// necessity; the handoff token is what stands in for it.
pub async fn handoff(
    State(state): State<AppState>,
    body: Result<axum::Json<HandoffRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // A malformed body is refused exactly as a wrong token is. `Json`'s own
    // rejection would say "Expected request with `Content-Type:
    // application/json`", which tells an unauthenticated caller that this path
    // exists and what it wants.
    let Ok(axum::Json(req)) = body else {
        return not_found();
    };
    if !state.handoff().redeem(&req.token) {
        return not_found();
    }
    json(
        &state,
        &HandoffResponse {
            token: state.policy().secret().expose_hex(),
        },
    )
}

type PathErr = axum::extract::rejection::PathRejection;

async fn blocking<T, F>(
    source: Arc<dyn crate::source::MeetingSource>,
    f: F,
) -> Result<T, SourceError>
where
    T: Send + 'static,
    F: FnOnce(&dyn crate::source::MeetingSource) -> Result<T, SourceError> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(source.as_ref())).await {
        Ok(result) => result,
        // The blocking task panicked. Surfacing the panic message would leak
        // whatever the panic quoted, which for a store error is a row.
        Err(_) => Err(SourceError::Backend("store task failed".into())),
    }
}

fn json<T: Serialize>(state: &AppState, value: &T) -> Response {
    let Ok(body) = serde_json::to_vec(value) else {
        return server_error();
    };
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(body))
        .expect("a JSON response is always constructible");
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    apply_security_headers(headers, state.csp());
    response
}

/// A bare 500. No body: a SQLite error string can quote the row it choked on,
/// and §10's never-log rules do not stop at the log file.
fn server_error() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::empty())
        .expect("a bare 500 is always constructible")
}

fn number(query: &str, key: &str) -> Option<u32> {
    query_param(query, key)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_is_capped_rather_than_trusted() {
        assert_eq!(number("limit=10", "limit"), Some(10));
        assert_eq!(
            number("limit=999999", "limit").unwrap().min(MAX_LIMIT),
            MAX_LIMIT
        );
        assert_eq!(number("limit=-1", "limit"), None);
        assert_eq!(number("limit=abc", "limit"), None);
        assert_eq!(number("", "limit"), None);
    }
}
