//! The post-meeting cleanup (#246): its progress, the switch between the
//! cleaned and the live transcript, and when it runs. Metadata only: no
//! transcript text passes through here.
use serde::{Deserialize, Serialize};

/// Where a meeting's cleanup is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupPhase {
    /// The meeting has no cleanup (off, a cloud meeting, or older).
    None,
    /// Queued or running.
    Pending,
    /// The cleaned version exists.
    Done,
    /// It stopped; the live version is shown.
    Failed,
}

/// Which transcript the meeting shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptVersion {
    /// What was transcribed during the meeting.
    Live,
    /// The post-meeting cleanup.
    Cleaned,
}

/// A meeting's cleanup, for the meeting view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupStatus {
    /// Where it is.
    pub phase: CleanupPhase,
    /// 0–100 while pending.
    pub percent: u8,
    /// The version shown, when it is one of the two.
    pub showing: Option<TranscriptVersion>,
    /// Why it failed, or what it left out (no speaker labels). Content-free.
    pub detail: Option<String>,
}

/// When cleanup runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CleanupMode {
    /// Only meetings no cloud provider transcribed (the default).
    Auto,
    /// Every meeting, Deepgram ones included.
    On,
    /// Never.
    Off,
}

/// The cleanup setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupSettingsDoc {
    /// When it runs.
    pub mode: CleanupMode,
}

/// A request to show one version.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowVersion {
    /// Which.
    pub version: TranscriptVersion,
}

/// Supplied by the daemon.
pub trait CleanupControl: std::fmt::Debug + Send + Sync {
    /// A meeting's cleanup, or `None` for an unknown meeting.
    fn status(&self, meeting: &str) -> Option<CleanupStatus>;
    /// Show `version`; `None` when the meeting has no such version.
    fn show(&self, meeting: &str, version: TranscriptVersion) -> Option<CleanupStatus>;
    /// The setting.
    fn settings(&self) -> CleanupSettingsDoc;
    /// Change the setting; applies to meetings stopped from now on.
    fn set_settings(&self, doc: CleanupSettingsDoc) -> Result<(), String>;
}

pub(crate) async fn status(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(control) = state.cleanup() else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || control.status(&id)).await {
        Ok(Some(status)) => crate::api::json(&state, &status),
        _ => crate::ingress::not_found(),
    }
}

pub(crate) async fn show(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Result<axum::Json<ShowVersion>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let (Some(control), Ok(axum::Json(request))) = (state.cleanup(), body) else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || control.show(&id, request.version)).await {
        Ok(Some(status)) => crate::api::json(&state, &status),
        _ => crate::ingress::not_found(),
    }
}

pub(crate) async fn get_settings(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> axum::response::Response {
    let Some(control) = state.cleanup() else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || control.settings()).await {
        Ok(doc) => crate::api::json(&state, &doc),
        Err(_) => crate::api::server_error(),
    }
}

pub(crate) async fn post_settings(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    body: Result<axum::Json<CleanupSettingsDoc>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let (Some(control), Ok(axum::Json(doc))) = (state.cleanup(), body) else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || {
        control.set_settings(doc).map(|()| control.settings())
    })
    .await
    {
        Ok(Ok(doc)) => crate::api::json(&state, &doc),
        Ok(Err(error)) => {
            let mut response = crate::api::json(&state, &serde_json::json!({ "error": error }));
            *response.status_mut() = axum::http::StatusCode::CONFLICT;
            response
        }
        Err(_) => crate::api::server_error(),
    }
}
