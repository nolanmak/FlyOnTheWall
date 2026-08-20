//! The seam between the UI and whatever actually opens an audio tap.
//!
//! `fotw-web` cannot start a recording and must not learn how. Core Audio
//! lives in `fotw-audio`, session lifecycle in `fotwd`, and `just seam` fails
//! the build if a `AudioDeviceID` or an `AudioBufferList` appears outside
//! `fotw-audio/src/platform/macos/`. So the web layer takes a trait, exactly
//! as it takes [`MeetingSource`](crate::source::MeetingSource) for the
//! library, and `fotwd` supplies the implementation that knows about devices.
//!
//! # Why the status is data and never a status code
//!
//! "Nothing is recording" is `200 {"state":"idle"}` rather than a 404 or a
//! 409. A status code that varied with recording state would answer *is this
//! person in a meeting right now* to anything that could reach the port, and
//! that is the one fact ING-09 is written to withhold. The HTTP layer says
//! only whether the request was well-formed and authorised; what the recorder
//! is doing is in the body.

use serde::{Deserialize, Serialize};

/// What the recorder is doing, as one word.
///
/// The UI switches on this, so its wire spelling is part of the contract
/// rather than an implementation detail — see the test that pins it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    /// No capture in flight.
    Idle,
    /// Capture is running.
    Recording,
}

/// What the recorder is doing, with the timings the UI renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingStatus {
    /// Idle or recording. Never absent, so the UI has no third case to guess.
    pub state: RecordingState,
    /// Epoch milliseconds capture began; `null` while idle.
    ///
    /// Deliberately `null` rather than absent or zero: the UI renders a
    /// "recording since HH:MM" from it, and zero is a value that renders.
    pub started_at_ms: Option<u64>,
    /// Milliseconds since capture began; `null` while idle.
    pub elapsed_ms: Option<u64>,
}

impl RecordingStatus {
    /// Nothing in flight.
    #[must_use]
    pub fn idle() -> Self {
        Self {
            state: RecordingState::Idle,
            started_at_ms: None,
            elapsed_ms: None,
        }
    }

    /// Capture running since `started_at_ms`.
    #[must_use]
    pub fn recording(started_at_ms: u64, elapsed_ms: u64) -> Self {
        Self {
            state: RecordingState::Recording,
            started_at_ms: Some(started_at_ms),
            elapsed_ms: Some(elapsed_ms),
        }
    }

    /// Whether capture is in flight.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.state == RecordingState::Recording
    }
}

/// Why a start or a stop did not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    /// Start was called while capture was already running.
    ///
    /// Not a failure the user needs to see — a double-clicked button produces
    /// it — but the recorder must not open a second tap on the same device.
    #[error("already recording")]
    AlreadyRecording,

    /// Stop was called with nothing in flight.
    #[error("not recording")]
    NotRecording,

    /// The device, the library or the consent gate refused.
    #[error("{0}")]
    Failed(String),
}

/// Anything that can start and stop a recording on the UI's behalf.
///
/// `Send + Sync + 'static` because the handlers hand it to
/// [`tokio::task::spawn_blocking`]: opening a Core Audio tap blocks, and
/// blocking a runtime worker would stall the live delta stream of the very
/// meeting being started.
pub trait RecorderControl: Send + Sync + 'static {
    /// Begin capturing, returning the state that results.
    ///
    /// The implementation owns the consent obligations that survive the HTTP
    /// layer — the audit entry (CON-08) and the jurisdiction escalation
    /// (CON-05) — because they must hold for the AppKit shell and the CLI too,
    /// and a rule enforced in a handler is a rule the next caller forgets.
    ///
    /// # Errors
    ///
    /// [`RecorderError::AlreadyRecording`] if capture is in flight, or
    /// [`RecorderError::Failed`] if the device or the library refused.
    fn start(&self) -> Result<RecordingStatus, RecorderError>;

    /// Stop capturing and finalize the session.
    ///
    /// # Errors
    ///
    /// [`RecorderError::NotRecording`] if nothing is in flight, or
    /// [`RecorderError::Failed`] if finalizing failed.
    fn stop(&self) -> Result<RecordingStatus, RecorderError>;

    /// What is happening right now. Must not block for long: the UI polls it.
    fn status(&self) -> RecordingStatus;
}
