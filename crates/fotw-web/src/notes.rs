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

fn respond(state: &AppState, result: Result<NoteDocument, NoteError>) -> Response {
    match result {
        Ok(doc) => api::json(state, &doc),
        Err(NoteError::NotFound) => not_found(),
        Err(NoteError::Backend) => api::server_error(),
        Err(error) => {
            let (status, code) = match error {
                NoteError::Conflict => (StatusCode::CONFLICT, "revision_conflict"),
                _ => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_note"),
            };
            let mut response = api::json(state, &serde_json::json!({"error":code}));
            *response.status_mut() = status;
            response
        }
    }
}
