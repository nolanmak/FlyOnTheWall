//! Content-free recorder state for the native shell. No native APIs or capture here.
use fotw_web::recorder::{
    CaptureDelivery, CaptureSource, RecordingState, RecordingStatus, SpeechState,
};

/// The native surfaces' projection of an authoritative recorder observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderView {
    /// Actual recorder phase, independent of the last button press.
    pub state: RecordingState,
    /// Short visible state label.
    pub title: &'static str,
    /// Capture time, absent until the recorder reports a real clock.
    pub elapsed_label: Option<String>,
    /// Source health and actual active transcription provenance.
    pub detail: String,
    /// Current failure, independently of whether capture remains active.
    pub error: Option<String>,
    /// Whether a new Stop command may be submitted.
    pub stop_enabled: bool,
}

impl RecorderView {
    /// Project a recorder snapshot without starting a clock or guessing a provider.
    #[must_use]
    pub fn from_status(status: &RecordingStatus) -> Self {
        let (title, stop_enabled) = match status.state {
            RecordingState::Idle => ("Not recording", false),
            RecordingState::Preparing | RecordingState::Starting
                if status.preparation_cancel_requested =>
            {
                ("Stopping", false)
            }
            RecordingState::Preparing => ("Preparing", true),
            RecordingState::Starting => ("Starting", true),
            RecordingState::Recording => ("Recording", true),
            RecordingState::Finishing => ("Finishing", false),
        };
        let elapsed_label = if matches!(
            status.state,
            RecordingState::Recording | RecordingState::Finishing
        ) {
            status
                .elapsed_ms
                .map(|ms| fotw_shell::format_elapsed(std::time::Duration::from_millis(ms)))
        } else {
            None
        };
        Self {
            state: status.state,
            title,
            elapsed_label,
            detail: details(status),
            error: errors(status),
            stop_enabled,
        }
    }
}

fn errors(status: &RecordingStatus) -> Option<String> {
    let mut messages = Vec::new();
    if let Some(error) = status
        .transcription_error
        .as_ref()
        .filter(|e| !e.trim().is_empty())
    {
        messages.push(error.clone());
    }
    for source in &status.speech_status {
        if let Some(error) = source.error.as_ref().filter(|e| !e.trim().is_empty()) {
            if messages.iter().any(|existing| existing == error) {
                continue;
            }
            let name = match source.source {
                CaptureSource::System => "System",
                CaptureSource::Mic => "Mic",
            };
            messages.push(format!("{name}: {error}"));
        }
    }
    if messages.is_empty() {
        None
    } else {
        Some(messages.join(" · "))
    }
}

fn details(status: &RecordingStatus) -> String {
    let mut parts = Vec::new();
    if let Some(health) = status.capture_health {
        for (name, source) in [("System audio", health.system), ("Microphone", health.mic)] {
            let state = match source.state {
                CaptureDelivery::Healthy => continue,
                CaptureDelivery::Confirming => "confirming delivery",
                CaptureDelivery::Quiet => "quiet",
                CaptureDelivery::Recovering => "recovering",
                CaptureDelivery::Exhausted => "needs retry",
                CaptureDelivery::CleanupBlocked => "cleanup blocked",
                CaptureDelivery::Stopped => "stopped",
                CaptureDelivery::Unavailable => "unavailable",
            };
            parts.push(format!("{name}: {state}"));
        }
    }
    for source in &status.speech_status {
        let name = match source.source {
            CaptureSource::System => "System",
            CaptureSource::Mic => "Mic",
        };
        let provider = if source.provider.trim().is_empty() {
            "unknown engine"
        } else {
            &source.provider
        };
        let model = if source.model.trim().is_empty() {
            String::new()
        } else {
            format!(" ({})", source.model)
        };
        let state = match source.state {
            SpeechState::Cloud | SpeechState::Local | SpeechState::Open => "",
            SpeechState::Connecting => "; connecting",
            SpeechState::Reconnecting => "; reconnecting",
            SpeechState::Closed => "; closed",
            SpeechState::Complete => "; complete",
            SpeechState::Incomplete => "; incomplete",
        };
        let lag = source
            .pending_audio_ms
            .filter(|ms| *ms >= 1000)
            .map_or_else(String::new, |ms| {
                format!("; {} s pending", ms.div_ceil(1000))
            });
        parts.push(format!("{name}: {provider}{model}{state}{lag}"));
    }
    if parts.is_empty() && status.is_active() {
        parts.push("Transcription status unavailable".into());
    }
    parts.join(" · ")
}
