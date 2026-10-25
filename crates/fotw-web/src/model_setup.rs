//! Explicit model provisioning. The web layer never reads weights or credentials.
use serde::{Deserialize, Serialize};
/// Pinned model metadata; availability is only a file check, not native readiness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    /// Catalog name accepted by setup and transcription settings.
    pub name: String,
    /// Language and quantization facts, not an accuracy recommendation.
    pub description: String,
    /// Exact expected artifact size.
    pub bytes: u64,
    /// Lowercase SHA-256 of the published bytes.
    pub sha256: String,
    /// Immutable publisher revision.
    pub revision: String,
    /// Artifact license identifier.
    pub license: String,
    /// Pinned publisher model card containing the license.
    pub license_url: String,
    /// An ordinary file exists; this does not claim verification or readiness.
    pub present: bool,
}
/// An owned setup job's state, retained across browser navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No setup has run in this daemon.
    Idle,
    /// Inspecting existing installed bytes.
    Checking,
    /// Streaming download or import into a separate partial file.
    Transferring,
    /// Comparing size and checksum.
    Verifying,
    /// Publication has started; cancellation is no longer admitted.
    Installing,
    /// File verification succeeded; native readiness remains separate.
    Verified,
    /// Cancellation requested, but IO or source destruction still owns the job.
    Stopping,
    /// Cancellation finished and admission has been released.
    Cancelled,
    /// Setup failed and can be retried explicitly.
    Failed,
}
/// No model contents, audio or credentials are returned.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    /// Unique job identity across daemon restarts; empty means no job.
    pub id: String,
    /// Catalog name for this job.
    pub model: Option<String>,
    /// Current work stage.
    pub phase: Phase,
    /// Actual bytes transferred or verified.
    pub bytes: u64,
    /// Pinned expected size.
    pub total: u64,
    /// Retry remains excluded until all owned IO and teardown returns.
    pub worker_running: bool,
    /// Actionable setup result or refusal, never model contents.
    pub message: Option<String>,
}
impl Default for Job {
    fn default() -> Self {
        Self {
            id: String::new(),
            model: None,
            phase: Phase::Idle,
            bytes: 0,
            total: 0,
            worker_running: false,
            message: None,
        }
    }
}
/// Catalog plus the current or most recent job.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    /// Known pinned artifacts with lightweight file availability.
    pub models: Vec<Model>,
    /// Current daemon-owned job, including terminal outcomes.
    pub job: Job,
}
/// Source access is permitted only after an explicit authenticated action.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
pub enum Request {
    /// Explicitly download catalog bytes over HTTPS.
    Download {
        /// Known catalog name.
        model: String,
    },
    /// Copy and verify bytes already available locally.
    Import {
        /// Known catalog name.
        model: String,
        /// Absolute local input path, including cache symlinks.
        path: String,
    },
}
impl Request {
    /// Requested catalog name; validation belongs to the daemon.
    pub fn model(&self) -> &str {
        match self {
            Self::Download { model } | Self::Import { model, .. } => model,
        }
    }
}
/// Stale tabs cannot cancel a replacement job.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    /// Identity observed by the requesting tab.
    pub job_id: String,
}
/// Owned daemon work, independent of HTTP request lifetimes.
pub trait ModelSetupControl: std::fmt::Debug + Send + Sync {
    /// Inspect metadata without downloading, loading a model or reading keys.
    fn read(&self) -> Status;
    /// Admit one owned job and return without waiting for transfer completion.
    fn start(&self, request: Request) -> Result<Status, String>;
    /// Fence publication if the same job has not entered atomic installation.
    fn cancel(&self, job_id: &str) -> Result<Status, String>;
}

pub(crate) async fn read(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> axum::response::Response {
    let Some(control) = state.model_setup() else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || control.read()).await {
        Ok(status) => crate::api::json(&state, &status),
        Err(_) => crate::api::server_error(),
    }
}
pub(crate) async fn start(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    body: Result<axum::Json<Request>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let (Some(control), Ok(axum::Json(request))) = (state.model_setup(), body) else {
        return crate::ingress::not_found();
    };
    respond(
        &state,
        tokio::task::spawn_blocking(move || control.start(request)).await,
    )
}
pub(crate) async fn cancel(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    body: Result<axum::Json<Cancel>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let (Some(control), Ok(axum::Json(request))) = (state.model_setup(), body) else {
        return crate::ingress::not_found();
    };
    respond(
        &state,
        tokio::task::spawn_blocking(move || control.cancel(&request.job_id)).await,
    )
}
fn respond(
    state: &crate::AppState,
    result: Result<Result<Status, String>, tokio::task::JoinError>,
) -> axum::response::Response {
    match result {
        Ok(Ok(status)) => crate::api::json(state, &status),
        Ok(Err(error)) => {
            let mut response = crate::api::json(state, &serde_json::json!({"error":error}));
            *response.status_mut() = axum::http::StatusCode::CONFLICT;
            response
        }
        Err(_) => crate::api::server_error(),
    }
}
