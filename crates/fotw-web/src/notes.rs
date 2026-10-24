//! Authenticated, revision-checked notes. Content stays in the encrypted store.
use crate::{AppState, api, ingress::not_found};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
};
use serde::{Deserialize, Serialize};

/// A recording-relative first-keystroke anchor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoteAnchor {
    /// Stable identity. Older clients may omit it when creating an anchor.
    #[serde(default)]
    pub id: Option<String>,
    /// Current block position.
    pub block_idx: i64,
    /// Text associated with that block.
    pub block_text: String,
    /// Offset from the original recording start, never display-relative.
    pub typed_at_ms: i64,
}

/// Snapshot for load/save. On save `revision` is the version the editor loaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoteDocument {
    /// Zero for an absent note; otherwise the last observed revision.
    pub revision: i64,
    /// Untrusted Markdown, rendered through text nodes in the browser.
    pub body_md: String,
    /// Original recording offsets.
    pub anchors: Vec<NoteAnchor>,
}

/// Errors intentionally contain no note text or database diagnostics.
#[derive(Debug)]
pub enum NoteError {
    /// Unknown meeting or unsupported adapter.
    NotFound,
    /// Another writer won; explicit user resolution is required.
    Conflict,
    /// Bounded document/anchor validation failed.
    Invalid,
    /// Capture has left the recorder but finalization has not published its clock.
    Unavailable,
    /// Internal failure.
    Backend,
}

pub(crate) async fn load(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let source = state.source().clone();
    respond(
        &state,
        tokio::task::spawn_blocking(move || source.note(&id))
            .await
            .unwrap_or(Err(NoteError::Backend)),
    )
}

pub(crate) async fn save(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<axum::Json<NoteDocument>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(axum::Json(doc)) = body else {
        return not_found();
    };
    let source = state.source().clone();
    respond(
        &state,
        tokio::task::spawn_blocking(move || source.save_note(&id, doc))
            .await
            .unwrap_or(Err(NoteError::Backend)),
    )
}

fn respond<T: Serialize>(state: &AppState, result: Result<T, NoteError>) -> Response {
    match result {
        Ok(doc) => api::json(state, &doc),
        Err(NoteError::NotFound) => not_found(),
        Err(NoteError::Backend) => api::server_error(),
        Err(error) => {
            let (status, code) = match error {
                NoteError::Conflict => (StatusCode::CONFLICT, "revision_conflict"),
                NoteError::Unavailable => (StatusCode::CONFLICT, "recording_clock_unavailable"),
                _ => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_note"),
            };
            let mut response = api::json(state, &serde_json::json!({"error":code}));
            *response.status_mut() = status;
            response
        }
    }
}

/// A server-owned capture clock. Never deserialized from a request.
#[derive(Debug, Clone, Copy)]
pub struct SessionClock {
    /// Epoch milliseconds at capture start.
    pub started_at_ms: i64,
    /// Elapsed capture milliseconds, frozen after Stop.
    pub elapsed_ms: i64,
}

/// A durable session draft, including its saved-meeting destination when known.
#[derive(Debug, Serialize)]
pub struct SessionNote {
    /// Original capture identity.
    pub session_id: String,
    /// Saved meeting after successful finalization.
    pub meeting_id: Option<String>,
    /// Original recording start.
    pub started_at_ms: i64,
    /// Authoritative bound; absent for an orphan draft awaiting recovery.
    pub elapsed_ms: Option<i64>,
    /// Revision-checked editor document.
    pub note: NoteDocument,
}

fn session_clock(state: &AppState, id: &str) -> Option<SessionClock> {
    let status = state.recorder()?.status();
    if status.session_id.as_deref() != Some(id) {
        return None;
    }
    Some(SessionClock {
        started_at_ms: i64::try_from(status.started_at_ms?).ok()?,
        elapsed_ms: i64::try_from(status.elapsed_ms?).ok()?,
    })
}

pub(crate) async fn load_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let source = state.source().clone();
    let context = state.clone();
    respond(
        &state,
        tokio::task::spawn_blocking(move || {
            let clock = session_clock(&context, &id);
            source.session_note(&id, clock)
        })
        .await
        .unwrap_or(Err(NoteError::Backend)),
    )
}

pub(crate) async fn save_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<axum::Json<NoteDocument>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(axum::Json(doc)) = body else {
        return not_found();
    };
    let source = state.source().clone();
    let context = state.clone();
    respond(
        &state,
        tokio::task::spawn_blocking(move || {
            let clock = session_clock(&context, &id);
            source.save_session_note(&id, doc, clock)
        })
        .await
        .unwrap_or(Err(NoteError::Backend)),
    )
}

/// Metadata only; note text is fetched separately after selecting a draft.
#[derive(Debug, Serialize)]
pub struct DraftSummary {
    /// Original capture identity.
    pub session_id: String,
    /// Capture start, in epoch milliseconds.
    pub started_at_ms: i64,
    /// Last successful note save, in epoch milliseconds.
    pub updated_at_ms: i64,
}

pub(crate) async fn list_drafts(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let query = uri.query().unwrap_or_default();
    let limit = crate::query::query_param(query, "limit")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(50)
        .min(100);
    let offset = crate::query::query_param(query, "offset")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let source = state.source().clone();
    let result = tokio::task::spawn_blocking(move || source.note_drafts(limit, offset))
        .await
        .unwrap_or(Err(NoteError::Backend));
    respond(
        &state,
        result.map(|drafts| serde_json::json!({"drafts":drafts})),
    )
}
