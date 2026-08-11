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
use crate::ingress::not_found;
use crate::query::query_param;
use crate::source::{Hit, MeetingDetail, MeetingRow, SourceError};
use crate::state::AppState;
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
    /// Goes in `Authorization: Bearer`, and into `sessionStorage` — never a
    /// cookie (ING-08).
    pub token: String,
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
