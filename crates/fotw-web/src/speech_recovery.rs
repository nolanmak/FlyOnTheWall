//! Saved-audio retry control. Contains metadata, never audio or transcript text.
use serde::{Deserialize, Serialize};

/// State of a local-only saved-transcription job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    /// Audio is indexed with an unknown final clock. Only independently
    /// validated canonical prefixes with a saved model plan may be retried.
    Retained,
    /// Available saved speech was recovered; original audio/time remains unresolved.
    Partial,
    /// Retained canonical audio has unfinished transcription.
    Incomplete,
    /// Resolving the original model and source ownership.
    Preparing,
    /// Recovering pending source intervals locally.
    Transcribing,
    /// Cancellation/deadline has fenced publication, but ownership remains.
    Stopping,
    /// Every source is complete and projected into the saved meeting.
    Complete,
    /// Retry failed; original audio remains available.
    Failed,
    /// User cancelled the retry; original audio remains available.
    Cancelled,
}

/// Pending coverage for one physical capture source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySource {
    /// Original capture source, system or mic.
    pub source: String,
    /// Saved local model identity.
    pub model: String,
    /// Saved installed revision, when recorded.
    pub revision: Option<String>,
    /// Uncommitted canonical duration; excludes unmapped original audio and is
    /// unrelated to recognized word count or whole-recording completeness.
    pub pending_audio_ms: u64,
}

/// Content-free job status returned to an authenticated dashboard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryStatus {
    /// Current recovery stage.
    pub phase: RecoveryPhase,
    /// Saved source plans and pending intervals.
    pub sources: Vec<RecoverySource>,
    /// Owned initialization/inference/cleanup has not yet returned.
    pub worker_running: bool,
    /// Another explicit retry can currently be admitted.
    pub can_retry: bool,
    /// Actionable, content-free explanation.
    pub message: Option<String>,
}

/// Refusals do not expose filesystem paths, database errors or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryError {
    /// No recoverable saved capture belongs to this meeting.
    NotFound,
    /// A recovery or native capture still owns the operation.
    Busy,
    /// Saved state could not be validated.
    Invalid,
}

/// Supplied by the daemon; the web crate cannot load a speech model.
pub trait SpeechRecoveryControl: std::fmt::Debug + Send + Sync {
    /// Inspect saved metadata or an active job without starting inference.
    fn read(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError>;
    /// Admit a local-only retry and return immediately with owned job status.
    fn retry(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError>;
    /// Revoke publication, retaining admission until native work really returns.
    fn cancel(&self, meeting: &str) -> Result<RecoveryStatus, RecoveryError>;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Action {}

pub(crate) async fn read(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(control) = state.speech_recovery() else {
        return crate::ingress::not_found();
    };
    respond(
        &state,
        tokio::task::spawn_blocking(move || control.read(&id))
            .await
            .unwrap_or(Err(RecoveryError::Invalid)),
    )
}
pub(crate) async fn retry(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Result<axum::Json<Action>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    if body.is_err() {
        return crate::ingress::not_found();
    }
    let Some(control) = state.speech_recovery() else {
        return crate::ingress::not_found();
    };
    respond(
        &state,
        tokio::task::spawn_blocking(move || control.retry(&id))
            .await
            .unwrap_or(Err(RecoveryError::Invalid)),
    )
}
pub(crate) async fn cancel(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Result<axum::Json<Action>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    if body.is_err() {
        return crate::ingress::not_found();
    }
    let Some(control) = state.speech_recovery() else {
        return crate::ingress::not_found();
    };
    respond(
        &state,
        tokio::task::spawn_blocking(move || control.cancel(&id))
            .await
            .unwrap_or(Err(RecoveryError::Invalid)),
    )
}
fn respond(
    state: &crate::AppState,
    result: Result<RecoveryStatus, RecoveryError>,
) -> axum::response::Response {
    match result {
        Ok(status) => crate::api::json(state, &status),
        Err(RecoveryError::NotFound) => crate::ingress::not_found(),
        Err(error) => {
            let mut response = crate::api::json(
                state,
                &serde_json::json!({"error": if error == RecoveryError::Busy { "recovery_busy" } else { "saved_audio_unavailable" }}),
            );
            *response.status_mut() = axum::http::StatusCode::CONFLICT;
            response
        }
    }
}
