//! The shell's state machine. No AppKit, no clock, no I/O.
//!
//! `NSApplication::run()` never returns and owns the main thread, so anything
//! that lives behind it is untestable on a CI runner with no window server.
//! Everything that decides *what the shell shows* therefore lives here, as a
//! plain Rust type driven by [`ShellInput`], and the AppKit layer is a
//! renderer that applies [`ShellCore::view`] and forwards
//! [`ShellEffect`]s. This is the only reason any of the shell is testable
//! (docs/REQUIREMENTS.md 5.6).
//!
//! # The phases
//!
//! ```text
//!                  Start                StopRequested            StopCompleted
//!   Idle ────────────────────► Recording ──────────────► Finishing ─────────────► Finished
//!    ▲                            │                          │                       │
//!    │                            │ CaptureFailed            │ CaptureFailed         │ linger
//!    │                            ▼                          ▼                       │ or Dismiss
//!    └──────────────── Dismiss ─ Faulted ◄───────────────────┘                       │
//!    └──────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! `Finishing` is not decoration. Stop is asynchronous — the WAL is flushed
//! and the muxer closed after capture ends — and **the indicator must stay up
//! for that whole window**, because bytes from the meeting are still being
//! written. A shell that hid the pill on the button press would show nothing
//! during the one part of teardown that can still fail.

use std::time::Duration;

use crate::clock::{Monotonic, format_elapsed};
use crate::view::{
    Level, MenuAction, MenuButton, MenuModel, PillView, ShellView, Tone, TrayState, TrayView,
};

/// How long the pill lingers on `Saved` before the shell returns to idle.
///
/// Long enough to read, short enough not to be furniture.
pub const FINISHED_LINGER: Duration = Duration::from_secs(4);

/// How many segments the pill's level meter has.
pub const METER_SEGMENTS: usize = 6;

/// Where the shell is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing is being captured.
    Idle,
    /// Capture is live.
    Recording {
        /// When the session started.
        started: Monotonic,
        /// Wall time since `started`, as of the last tick.
        elapsed: Duration,
    },
    /// Capture has stopped; the session is still being written.
    Finishing {
        /// Final length of the session. Frozen — the clock stopped with capture.
        elapsed: Duration,
    },
    /// The session is closed. Returns to [`Phase::Idle`] after [`FINISHED_LINGER`].
    Finished {
        /// Final length of the session.
        elapsed: Duration,
        /// When the session finished, for the linger timer.
        at: Monotonic,
    },
    /// Capture failed. Stays until the user acknowledges it.
    Faulted {
        /// How much was captured before the failure.
        elapsed: Duration,
        /// What went wrong, as reported by the capture layer.
        reason: String,
    },
}

impl Phase {
    /// Whether this phase means "there is nothing to indicate".
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Whether capture is live.
    #[must_use]
    pub const fn is_recording(&self) -> bool {
        matches!(self, Self::Recording { .. })
    }

    /// Session length so far, or zero when idle.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        match self {
            Self::Idle => Duration::ZERO,
            Self::Recording { elapsed, .. }
            | Self::Finishing { elapsed }
            | Self::Finished { elapsed, .. }
            | Self::Faulted { elapsed, .. } => *elapsed,
        }
    }
}

/// Everything that can move the shell.
///
/// Deliberately **not** `#[non_exhaustive]`: `tests/con02_indicator.rs`
/// matches this enum exhaustively to prove that no input takes the indicator
/// down while capture is live. Marking it `non_exhaustive` would force that
/// test to add a wildcard arm and quietly stop covering new inputs — which is
/// exactly the hole a "hide the pill" input would slip through.
#[derive(Clone, Debug, PartialEq)]
pub enum ShellInput {
    /// The user asked to start a session.
    Start {
        /// Clock reading at the moment of the request.
        at: Monotonic,
    },
    /// The clock advanced.
    Tick {
        /// Current clock reading.
        now: Monotonic,
    },
    /// A new input level was measured.
    Level(Level),
    /// The user asked to stop. Capture ends; the session is still being written.
    StopRequested,
    /// The capture layer reports the session is written and closed.
    StopCompleted {
        /// Clock reading at completion, for the linger timer.
        at: Monotonic,
    },
    /// The capture layer failed.
    CaptureFailed {
        /// What went wrong.
        reason: String,
    },
    /// The user acknowledged a finished or failed session.
    Dismiss,
}

/// Something the renderer or the host must do.
///
/// Everything *visual* is derived from [`ShellCore::view`] instead, so the
/// renderer cannot drift out of sync with the state by missing an effect.
/// These are the things that are not visual.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellEffect {
    /// Begin capturing.
    StartCapture,
    /// Tear the capture down.
    StopCapture,
    /// Start delivering [`ShellInput::Tick`].
    StartTicking,
    /// Stop delivering [`ShellInput::Tick`].
    StopTicking,
    /// Open the notes window.
    OpenNotes,
    /// Open the Disclosure Kit (CON-03).
    OpenDisclosureKit,
    /// Open settings.
    OpenSettings,
    /// Open the about box.
    OpenAbout,
    /// Quit the application.
    Quit,
}

/// The shell's state machine.
///
/// See the module docs. Construct with [`ShellCore::new`], drive with
/// [`ShellCore::handle`], render [`ShellCore::view`].
#[derive(Clone, Debug)]
pub struct ShellCore {
    phase: Phase,
    level: Level,
    /// Whether [`ShellEffect::StartCapture`] has been emitted without a
    /// matching [`ShellEffect::StopCapture`]. Not derivable from `phase`:
    /// when the capture layer reports its own completion we move out of
    /// `Recording` *without* commanding a stop it already performed.
    capture: bool,
    /// Whether ticks have been asked for. Tracked so the effects are edges,
    /// not level-triggered noise on every input.
    ticking: bool,
}

impl Default for ShellCore {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellCore {
    /// A shell that is not recording.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            level: Level::SILENT,
            capture: false,
            ticking: false,
        }
    }

    /// The current phase.
    #[must_use]
    pub const fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Whether the core believes capture is live.
    ///
    /// The CON-02 invariant is stated against this: whenever this is true,
    /// [`ShellCore::view`] returns a pill.
    #[must_use]
    pub const fn capture_is_live(&self) -> bool {
        self.capture
    }

    /// Whether the core wants [`ShellInput::Tick`] delivered.
    #[must_use]
    pub const fn is_ticking(&self) -> bool {
        self.ticking
    }

    /// Apply an input and report what the host must do.
    pub fn handle(&mut self, input: ShellInput) -> Vec<ShellEffect> {
        // The one input that changes state without changing phase. Kept out
        // of `next_phase` so that stays a pure phase function, and ignored
        // outside `Recording` so a level sampled during teardown cannot
        // repaint a meter the user is no longer being recorded by.
        if let ShellInput::Level(level) = input {
            if self.phase.is_recording() {
                self.level = level;
            }
            return Vec::new();
        }
        let next = self.next_phase(input);
        self.transition(next)
    }

    /// Translate a menu click.
    ///
    /// Rows that the current [`MenuModel`] reports as disabled are ignored, so
    /// a click that raced a phase change cannot re-enter a torn-down path.
    pub fn on_menu(&mut self, action: MenuAction, now: Monotonic) -> Vec<ShellEffect> {
        if !self.view().menu.button(action).enabled {
            return Vec::new();
        }
        match action {
            MenuAction::ToggleRecording => self.toggle(now),
            MenuAction::OpenNotes => vec![ShellEffect::OpenNotes],
            MenuAction::DisclosureKit => vec![ShellEffect::OpenDisclosureKit],
            MenuAction::Settings => vec![ShellEffect::OpenSettings],
            MenuAction::About => vec![ShellEffect::OpenAbout],
            MenuAction::Quit => vec![ShellEffect::Quit],
        }
    }

    /// Start if idle, stop if recording.
    ///
    /// A toggle arriving during `Finishing` is **dropped**, not queued: the
    /// previous session is still being flushed, and starting a new capture on
    /// top of that teardown is how you get two taps on one aggregate device.
    pub fn toggle(&mut self, now: Monotonic) -> Vec<ShellEffect> {
        match self.phase {
            Phase::Idle | Phase::Finished { .. } | Phase::Faulted { .. } => {
                self.handle(ShellInput::Start { at: now })
            }
            Phase::Recording { .. } => self.handle(ShellInput::StopRequested),
            Phase::Finishing { .. } => Vec::new(),
        }
    }

    /// What the shell shows right now.
    #[must_use]
    pub fn view(&self) -> ShellView {
        let pill = self.pill();
        ShellView {
            tray: self.tray(),
            menu: self.menu(),
            pill,
        }
    }

    // --- transitions -----------------------------------------------------

    fn next_phase(&self, input: ShellInput) -> Phase {
        match (&self.phase, input) {
            // Start. Idempotent while a session is live: a second click must
            // not restart the clock the user is watching.
            (
                Phase::Idle | Phase::Finished { .. } | Phase::Faulted { .. },
                ShellInput::Start { at },
            ) => Phase::Recording {
                started: at,
                elapsed: Duration::ZERO,
            },

            // The clock. Monotone by construction: `Monotonic::since`
            // saturates so a reading from before the session cannot wrap, and
            // the `max` means a clock that reads *backwards* freezes the
            // display instead of shrinking it. A meeting timer that counts
            // down is alarming and wrong, and the underlying clock does run
            // backwards in practice -- a coarse timer source, a suspended
            // process, a machine that slept.
            (Phase::Recording { started, elapsed }, ShellInput::Tick { now }) => Phase::Recording {
                started: *started,
                elapsed: (*elapsed).max(now.since(*started)),
            },

            // The linger timer. Only `Finished` auto-clears; a fault must be
            // acknowledged, because a session the user believes exists and
            // does not is exactly the thing worth interrupting them over.
            (Phase::Finished { elapsed, at }, ShellInput::Tick { now }) => {
                if now.since(*at) >= FINISHED_LINGER {
                    Phase::Idle
                } else {
                    Phase::Finished {
                        elapsed: *elapsed,
                        at: *at,
                    }
                }
            }

            // Stop. Capture ends here; the session is not closed yet.
            (Phase::Recording { elapsed, .. }, ShellInput::StopRequested) => {
                Phase::Finishing { elapsed: *elapsed }
            }

            // The capture layer reports the session is written.
            (
                Phase::Recording { elapsed, .. } | Phase::Finishing { elapsed },
                ShellInput::StopCompleted { at },
            ) => Phase::Finished {
                elapsed: *elapsed,
                at,
            },

            // Failure at any point in a session.
            (
                Phase::Recording { elapsed, .. } | Phase::Finishing { elapsed },
                ShellInput::CaptureFailed { reason },
            ) => Phase::Faulted {
                elapsed: *elapsed,
                reason,
            },

            // Acknowledgement.
            (Phase::Finished { .. } | Phase::Faulted { .. }, ShellInput::Dismiss) => Phase::Idle,

            // Everything else is a no-op: `Level` (handled by the caller
            // below), a stop with nothing running, a dismiss with nothing to
            // dismiss, a start while already recording.
            (phase, _) => phase.clone(),
        }
    }

    fn transition(&mut self, next: Phase) -> Vec<ShellEffect> {
        self.phase = next;
        let mut effects = Vec::new();

        // Capture edges.
        let want_capture = self.phase.is_recording();
        if want_capture && !self.capture {
            self.capture = true;
            effects.push(ShellEffect::StartCapture);
        } else if !want_capture && self.capture {
            self.capture = false;
            // Landing directly on `Finished` from `Recording` means the
            // capture layer reported its own completion. Do not command a
            // stop it has already performed. Every other exit from capture
            // (the Stop button, a fault) does need the teardown.
            if !matches!(self.phase, Phase::Finished { .. }) {
                effects.push(ShellEffect::StopCapture);
            }
        }

        // Tick edges. Invariant: ticking iff not idle. The linger timer and
        // the level meter both need the tick after capture has stopped.
        let want_tick = !self.phase.is_idle();
        if want_tick != self.ticking {
            self.ticking = want_tick;
            effects.push(if want_tick {
                ShellEffect::StartTicking
            } else {
                ShellEffect::StopTicking
            });
        }

        // A meter left showing the last frame of a finished meeting reads as
        // a live one.
        if !self.phase.is_recording() {
            self.level = Level::SILENT;
        }

        effects
    }

    // --- rendering -------------------------------------------------------

    fn pill(&self) -> Option<PillView> {
        let (status_label, tone, stop_enabled) = match &self.phase {
            Phase::Idle => return None,
            Phase::Recording { .. } => ("Recording", Tone::Live, true),
            Phase::Finishing { .. } => ("Finishing", Tone::Finishing, false),
            Phase::Finished { .. } => ("Saved", Tone::Saved, false),
            Phase::Faulted { .. } => ("Recording failed", Tone::Fault, false),
        };
        let elapsed = self.phase.elapsed();
        Some(PillView {
            elapsed,
            elapsed_label: format_elapsed(elapsed),
            status_label,
            level: self.level,
            tone,
            stop_enabled,
        })
    }

    fn tray(&self) -> TrayView {
        let state = match &self.phase {
            Phase::Idle | Phase::Finished { .. } => TrayState::Idle,
            Phase::Recording { .. } => TrayState::Recording,
            Phase::Finishing { .. } => TrayState::Finishing,
            Phase::Faulted { .. } => TrayState::Fault,
        };
        let elapsed_label = format_elapsed(self.phase.elapsed());
        let (title, tooltip) = match &self.phase {
            Phase::Idle => (None, "FlyOnTheWall — not recording".to_owned()),
            Phase::Recording { .. } => (
                Some(elapsed_label.clone()),
                format!("FlyOnTheWall — recording {elapsed_label}"),
            ),
            Phase::Finishing { .. } => (
                Some(elapsed_label.clone()),
                format!("FlyOnTheWall — saving {elapsed_label}"),
            ),
            Phase::Finished { .. } => (None, format!("FlyOnTheWall — saved {elapsed_label}")),
            Phase::Faulted { reason, .. } => {
                (None, format!("FlyOnTheWall — recording failed: {reason}"))
            }
        };
        TrayView {
            state,
            title,
            tooltip,
        }
    }

    fn menu(&self) -> MenuModel {
        let elapsed_label = format_elapsed(self.phase.elapsed());
        let (record_label, record_enabled, status) = match &self.phase {
            Phase::Idle => ("Start Recording", true, "Not recording".to_owned()),
            Phase::Recording { .. } => (
                "Stop Recording",
                true,
                format!("Recording — {elapsed_label}"),
            ),
            // Disabled: the stop is already in flight.
            Phase::Finishing { .. } => {
                ("Stop Recording", false, format!("Saving — {elapsed_label}"))
            }
            Phase::Finished { .. } => ("Start Recording", true, format!("Saved — {elapsed_label}")),
            Phase::Faulted { reason, .. } => (
                "Start Recording",
                true,
                format!("Recording failed: {reason}"),
            ),
        };
        MenuModel::build(
            MenuButton {
                label: record_label.to_owned(),
                enabled: record_enabled,
            },
            status,
        )
    }
}
