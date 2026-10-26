//! Application exclusions apply to the next recording's system-audio track.
use serde::{Deserialize, Serialize};

/// Explicit opt-in preferences. Missing rows use the legacy whole mix;
/// malformed or incomplete stored rows must never widen capture silently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureSettingsDoc {
    /// Exclude Spotify and its audio helpers from system capture.
    pub ignore_spotify: bool,
    /// Exclude all Brave audio, including any meetings in Brave.
    pub ignore_brave: bool,
}
impl CaptureSettingsDoc {
    /// Whether a filtered native scope is required.
    #[must_use]
    pub const fn filtered(self) -> bool {
        self.ignore_spotify || self.ignore_brave
    }
}

/// Saved next-recording choice, independent of any active meeting's snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureSettingsResponse {
    /// Absent on storage/validation failure, never inferred as whole mix.
    pub settings: Option<CaptureSettingsDoc>,
    /// Stable application exclusions are supported by this OS/runtime.
    pub supported: bool,
    /// Storage, capability or validation failure.
    pub error: Option<String>,
}

/// Daemon persistence seam; no capture or credentials in HTTP handlers.
pub trait CaptureControl: std::fmt::Debug + Send + Sync {
    /// Read saved preferences, without opening audio.
    fn read(&self) -> CaptureSettingsResponse;
    /// Save explicit preferences for the next recording only.
    fn set(&self, settings: CaptureSettingsDoc) -> Result<(), String>;
}

/// Authenticated GET. Native work never blocks a runtime worker.
pub async fn get(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> axum::response::Response {
    let Some(control) = state.capture() else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || control.read()).await {
        Ok(doc) => crate::api::json(&state, &doc),
        Err(_) => crate::api::server_error(),
    }
}

/// Authenticated POST, using the same ingress guard as other settings.
pub async fn post(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    body: Result<axum::Json<CaptureSettingsDoc>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let Some(control) = state.capture() else {
        return crate::ingress::not_found();
    };
    let Ok(axum::Json(doc)) = body else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || {
        let error = control.set(doc).err();
        let mut response = control.read();
        if error.is_some() {
            response.error = error;
        }
        response
    })
    .await
    {
        Ok(doc) => crate::api::json(&state, &doc),
        Err(_) => crate::api::server_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_exclusions_are_opt_in_and_independent() {
        assert!(!CaptureSettingsDoc::default().filtered());
        assert!(
            CaptureSettingsDoc {
                ignore_spotify: true,
                ignore_brave: false
            }
            .filtered()
        );
        assert!(
            CaptureSettingsDoc {
                ignore_spotify: false,
                ignore_brave: true
            }
            .filtered()
        );
    }
    #[test]
    fn incomplete_unknown_or_non_boolean_preferences_are_refused() {
        for raw in [
            r#"{}"#,
            r#"{"ignore_spotify":true}"#,
            r#"{"ignore_spotify":false,"ignore_brave":false,"ignore_chrome":true}"#,
            r#"{"ignore_spotify":"false","ignore_brave":false}"#,
        ] {
            assert!(
                serde_json::from_str::<CaptureSettingsDoc>(raw).is_err(),
                "{raw}"
            );
        }
    }
}
