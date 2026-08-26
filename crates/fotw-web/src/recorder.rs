//! The seam between the UI and whatever actually opens an audio tap.
//!
//! `fotw-web` cannot start a recording and must not learn how. Core Audio
//! lives in `fotw-audio`, session lifecycle in `fotwd`, and `just seam` fails
//! the build if any of the platform device or buffer types appears outside
//! `fotw-audio/src/platform/macos/` — this comment does not spell them for
//! exactly that reason. So the web layer takes a trait, exactly
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
    /// Capture has stopped; the meeting is still being written to the library.
    ///
    /// The name matches `fotw_shell::Phase::Finishing`, which has modelled
    /// this correctly since the menu bar shipped. The dashboard had only two
    /// words, so it spent the whole of finalization saying `recording` — a
    /// clock that kept climbing after Stop and a button that still offered to
    /// stop something (#77). The recorder's slot is occupied here, so a second
    /// Start is refused exactly as it is while recording.
    Finishing,
}

/// What the recorder is doing, with the timings the UI renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingStatus {
    /// Idle, recording or finishing. Never absent: the UI switches on this
    /// word and has no fourth case to guess.
    pub state: RecordingState,
    /// Epoch milliseconds capture began; `null` while idle.
    ///
    /// Deliberately `null` rather than absent or zero: the UI renders a
    /// "recording since HH:MM" from it, and zero is a value that renders.
    pub started_at_ms: Option<u64>,
    /// How long the meeting has run, in milliseconds; `null` while idle.
    ///
    /// While recording this counts up. While finishing it is *frozen* at the
    /// length the meeting ended on: capture has stopped, so the session cannot
    /// get any longer, and a number that kept moving would be a lie the user
    /// watches (#77).
    pub elapsed_ms: Option<u64>,
    /// Epoch milliseconds capture stopped; `null` unless finishing.
    ///
    /// Carried rather than derived from `started_at_ms + elapsed_ms` because
    /// the daemon genuinely holds it, and a client that reconstructed it would
    /// be reconstructing the one value this state exists to freeze. Defaulted
    /// so an older client's stored body still deserializes.
    #[serde(default)]
    pub ended_at_ms: Option<u64>,
    /// What the transcription provider last failed with, or `null`.
    ///
    /// Surfaced live rather than only at the end. Two Deepgram bugs each
    /// killed the stream on connect and produced nothing anywhere, so hours of
    /// audio were recorded beside an empty transcript that looked exactly like
    /// a quiet meeting. `null` means nothing has gone wrong; it is never an
    /// empty string, which the UI would render as a blank warning.
    #[serde(default)]
    pub transcription_error: Option<String>,
}

impl RecordingStatus {
    /// Nothing in flight.
    #[must_use]
    pub fn idle() -> Self {
        Self {
            state: RecordingState::Idle,
            started_at_ms: None,
            elapsed_ms: None,
            ended_at_ms: None,
            transcription_error: None,
        }
    }

    /// Capture running since `started_at_ms`.
    #[must_use]
    pub fn recording(started_at_ms: u64, elapsed_ms: u64) -> Self {
        Self {
            state: RecordingState::Recording,
            started_at_ms: Some(started_at_ms),
            elapsed_ms: Some(elapsed_ms),
            ended_at_ms: None,
            transcription_error: None,
        }
    }

    /// Capture stopped at `ended_at_ms`; the meeting is still being written.
    ///
    /// `elapsed_ms` is computed here rather than passed in so there is one
    /// place the frozen clock can come from, and it cannot disagree with the
    /// two timestamps beside it.
    #[must_use]
    pub fn finishing(started_at_ms: u64, ended_at_ms: u64) -> Self {
        Self {
            state: RecordingState::Finishing,
            started_at_ms: Some(started_at_ms),
            elapsed_ms: Some(ended_at_ms.saturating_sub(started_at_ms)),
            ended_at_ms: Some(ended_at_ms),
            transcription_error: None,
        }
    }

    /// Attach the provider's last failure, if there is one.
    #[must_use]
    pub fn with_transcription_error(mut self, error: Option<String>) -> Self {
        self.transcription_error = error.filter(|e| !e.trim().is_empty());
        self
    }

    /// Whether capture is in flight — a live tap, a running clock.
    ///
    /// Deliberately false while finishing: the microphone is closed and the
    /// session cannot get any longer. Callers asking "may I start?" want
    /// [`RecordingStatus::is_active`] instead.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.state == RecordingState::Recording
    }

    /// Whether the recorder's single slot is occupied.
    ///
    /// True while recording *and* while finishing: a Start arriving during
    /// finalization would open a second tap on the same device, so the slot
    /// stays taken until the meeting is on disk.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state != RecordingState::Idle
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
    /// Returns as soon as the tap is closed, not when the meeting is on disk:
    /// encoding a long call takes minutes and an HTTP handler cannot wait for
    /// it. The status that comes back is therefore
    /// [`RecordingState::Finishing`], with the clock frozen at the meeting's
    /// final length — an implementation that answers `Idle` here is claiming a
    /// meeting exists in the library before it does.
    ///
    /// # Errors
    ///
    /// [`RecorderError::NotRecording`] if nothing is in flight, or
    /// [`RecorderError::Failed`] if finalizing failed.
    fn stop(&self) -> Result<RecordingStatus, RecorderError>;

    /// What is happening right now. Must not block for long: the UI polls it.
    fn status(&self) -> RecordingStatus;
}
