//! Transcription settings. The web layer never loads a model or reads a key.
use serde::{Deserialize, Serialize};

/// Explicit recording policy. Absence or unknown values are invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeechMode {
    /// Audio streams to Deepgram when a key is configured.
    Deepgram,
    /// Audio stays on this computer for speech recognition.
    Local,
    /// Prefer Deepgram, then finish pending speech locally after an outage.
    Fallback,
}

/// Stored recording choice, shared with the daemon's STT row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechSettingsDoc {
    /// Explicit policy; never inferred from missing or malformed input.
    pub engine: SpeechMode,
    /// Known model name or absolute file path. Empty uses the daemon default.
    #[serde(default)]
    pub model: String,
    /// ISO language code, or empty/auto for model-supported detection.
    #[serde(default)]
    pub language: String,
    /// Live captions in local meetings (#245). On unless saved off.
    #[serde(default = "live_captions_default")]
    pub live_captions: bool,
}

fn live_captions_default() -> bool {
    true
}
impl SpeechSettingsDoc {
    /// Bound user input without changing its privacy policy.
    pub fn normalized(mut self) -> Result<Self, String> {
        self.model = self.model.trim().to_owned();
        self.language = self.language.trim().to_ascii_lowercase();
        if self.model.len() > 4096 || self.model.chars().any(char::is_control) {
            return Err(
                "Enter a model name or path without control characters (up to 4096 bytes).".into(),
            );
        }
        if self.language.len() > 16 || self.language.chars().any(char::is_control) {
            return Err("Enter a supported language code, or auto.".into());
        }
        if self.language == "auto" {
            self.language.clear();
        }
        Ok(self)
    }
}

/// Lightweight availability only; native readiness is established by Start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAvailability {
    /// The policy does not use a local model.
    NotRequired,
    /// The file exists; its contents and memory are checked on Start.
    PresentUnchecked,
    /// The selected file is absent.
    Missing,
    /// This platform has no supported native runtime.
    Unsupported,
    /// Saved settings are invalid, so no engine is inferred.
    Unknown,
}

/// Settings and actionable status. Contains no credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechSettingsResponse {
    /// None when the saved choice is unreadable. Never guessed as cloud.
    pub settings: Option<SpeechSettingsDoc>,
    /// File availability, never a promise that native allocation succeeds.
    pub model_availability: ModelAvailability,
    /// Resolved model path for local policies.
    pub model_path: Option<String>,
    /// Validation or storage failure; refused writes leave the old row intact.
    pub error: Option<String>,
    /// Whether the live-caption model (#245) is installed.
    #[serde(default)]
    pub captions_installed: bool,
}

/// The daemon supplies model validation and persistence outside web code.
pub trait SpeechControl: std::fmt::Debug + Send + Sync {
    /// Read the stored choice without model loading or credential lookup.
    fn read(&self) -> SpeechSettingsResponse;
    /// Validate the actual local model before saving. Applies next recording.
    fn set(&self, settings: SpeechSettingsDoc) -> Result<(), String>;
}

/// Authenticated GET. Native work never blocks a runtime worker.
pub async fn get(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> axum::response::Response {
    let Some(control) = state.speech() else {
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
    body: Result<axum::Json<SpeechSettingsDoc>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let Some(control) = state.speech() else {
        return crate::ingress::not_found();
    };
    let Ok(axum::Json(doc)) = body else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || {
        let error = doc.normalized().and_then(|valid| control.set(valid)).err();
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
