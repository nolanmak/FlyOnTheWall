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
use crate::prompt::{DetectedMeeting, PromptChoice, StartOrigin};
use crate::view::{
    Level, MenuAction, MenuButton, MenuModel, PillView, PromptView, ShellView, Tone, TrayState,
    TrayView,
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
        /// Which human action this was. Written to the audit log; there is no
        /// variant meaning "nobody asked" (CON-01).
        origin: StartOrigin,
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

    /// The detector believes a meeting is in progress.
    ///
    /// **This arms. It does not record.** The only thing it may do is put a
    /// prompt on screen; see `tests/con01_detection_arms_only.rs`, which
    /// exists to make that permanent.
    MeetingDetected {
        /// Clock reading when the detector fired.
        at: Monotonic,
        /// What was detected, and the evidence for it.
        meeting: DetectedMeeting,
    },

    /// The detector's signals went away before the user answered.
    ///
    /// The call ended, the app quit, the mic went cold. The prompt comes down
    /// rather than sitting there offering to record a meeting that is over.
    DetectionCleared,

    /// The user answered a detection prompt.
    PromptResponse {
        /// Clock reading at the click.
        at: Monotonic,
        /// What they chose.
        choice: PromptChoice,
    },

    /// The user ticked or cleared the prompt's all-party acknowledgement
    /// (CON-05).
    ///
    /// **This starts nothing.** It is the checkbox, not the button: all it
    /// can do is make [`PromptView::start_enabled`] true, and a person still
    /// has to press Start afterwards. Ignored when no prompt is on screen, so
    /// a click that raced a withdrawal cannot leave a tick waiting to
    /// pre-authorise the next meeting.
    PromptAcknowledged {
        /// Whether the box is now ticked.
        acknowledged: bool,
    },
}

/// Something the renderer or the host must do.
///
/// Everything *visual* is derived from [`ShellCore::view`] instead, so the
/// renderer cannot drift out of sync with the state by missing an effect.
/// These are the things that are not visual.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEffect {
    /// Write the user-initiated Start to the local audit log (CON-01, CON-08).
    ///
    /// Emitted **immediately before** [`ShellEffect::StartCapture`], in the
    /// same batch, so the log records the intent even if capture then fails.
    /// CON-01's acceptance criterion is stated against this log, which is why
    /// it is an effect the host must handle rather than a line of logging
    /// inside the capture layer.
    AuditStart(StartOrigin),
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
    /// The user said "not now": stop prompting for a while.
    ///
    /// How long is the detector's business, not the shell's. What matters
    /// here is that "not now" cannot mean "ask again in four seconds", which
    /// is how a consent surface gets habituated into invisibility.
    SnoozeDetection,
    /// The user said "never for this app": persist that, keyed by app.
    SuppressApp(String),
}

/// The shell's state machine.
///
/// See the module docs. Construct with [`ShellCore::new`], drive with
/// [`ShellCore::handle`], render [`ShellCore::view`].
#[derive(Clone, Debug)]
pub struct ShellCore {
    phase: Phase,
    level: Level,
    /// The armed meeting-detection prompt, if one is on screen.
    ///
    /// Deliberately *not* a [`Phase`]: arming is not a state of the recorder,
    /// it is a question being asked of the user while the recorder does
    /// nothing at all. Modelling it as a phase would put "we are nearly
    /// recording" into the same type as "we are recording", which is the
    /// distinction this whole feature exists to keep sharp.
    prompt: Option<DetectedMeeting>,
    /// Whether the all-party acknowledgement on that prompt is ticked
    /// (CON-05).
    ///
    /// Lives here rather than in the checkbox on screen so that clearing it is
    /// a state transition with a test, not an `NSButton` somebody has to
    /// remember to reset. Every path that changes which meeting is being asked
    /// about clears it: an acknowledgement is about *this* call and the people
    /// on it.
    acknowledged: bool,
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
            prompt: None,
            acknowledged: false,
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

    /// What the detector has armed, if anything.
    #[must_use]
    pub const fn armed(&self) -> Option<&DetectedMeeting> {
        self.prompt.as_ref()
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

        // The detection inputs. None of them reaches `next_phase`, which is
        // the mechanical reason detection cannot start a recording: the only
        // one that can move the phase is a click on the prompt, and it goes
        // through the same `Start` path as the menu row.
        match input {
            ShellInput::MeetingDetected { meeting, .. } => {
                // Only from idle. A prompt raised during a live session, or
                // during the flush that follows one, would either be
                // meaningless or race the next session's start.
                if self.phase.is_idle() {
                    // A *different* meeting is a different consent question,
                    // so the tick goes. An identical one is the same question
                    // asked twice -- the detector holds rather than re-arming,
                    // but a core that cleared the tick on every repeat would
                    // let a chatty detector make the box impossible to keep
                    // ticked, and Start impossible to reach.
                    if self.prompt.as_ref() != Some(&meeting) {
                        self.acknowledged = false;
                    }
                    self.prompt = Some(meeting);
                }
                return Vec::new();
            }
            ShellInput::DetectionCleared => {
                self.withdraw_prompt();
                return Vec::new();
            }
            ShellInput::PromptResponse { at, choice } => return self.answer_prompt(choice, at),
            ShellInput::PromptAcknowledged { acknowledged } => {
                // Only while something is being asked. A tick that arrives
                // after the prompt came down would otherwise sit there and
                // pre-acknowledge the next meeting, which is an
                // acknowledgement nobody gave.
                if self.prompt.is_some() {
                    self.acknowledged = acknowledged;
                }
                return Vec::new();
            }
            _ => {}
        }

        let origin = match &input {
            ShellInput::Start { origin, .. } => Some(*origin),
            _ => None,
        };
        let next = self.next_phase(input);
        self.transition(next, origin)
    }

    /// Apply the user's answer to a detection prompt.
    ///
    /// Returns nothing at all when there is no prompt on screen. A response
    /// can arrive for a prompt that was already withdrawn — the call ended,
    /// the detector cleared, the click was in flight — and honouring it would
    /// start a recording nobody is looking at.
    fn answer_prompt(&mut self, choice: PromptChoice, at: Monotonic) -> Vec<ShellEffect> {
        let Some(meeting) = self.prompt.clone() else {
            return Vec::new();
        };
        match choice {
            PromptChoice::Start { acknowledged } => {
                // CON-05: an all-party or contested jurisdiction requires an
                // explicit acknowledgement. Leaving the prompt up rather than
                // starting is the whole content of "blocking".
                //
                // Either source counts: the checkbox this core is tracking, or
                // a caller that asserts it directly (the web UI, which draws
                // its own). Neither can be inferred -- a missing answer is a
                // "no", which is why this is `||` over two explicit `true`s
                // and not a default.
                if meeting.requires_acknowledgement && !(acknowledged || self.acknowledged) {
                    return Vec::new();
                }
                self.withdraw_prompt();
                let origin = StartOrigin::DetectionPrompt;
                let next = self.next_phase(ShellInput::Start { at, origin });
                self.transition(next, Some(origin))
            }
            PromptChoice::NotNow => {
                self.withdraw_prompt();
                vec![ShellEffect::SnoozeDetection]
            }
            PromptChoice::NeverForThisApp => {
                self.withdraw_prompt();
                vec![ShellEffect::SuppressApp(meeting.app_key)]
            }
        }
    }

    /// Take the prompt down, and the acknowledgement with it.
    ///
    /// One function rather than two assignments at six call sites: the pair
    /// must move together, and the failure mode of forgetting the second one
    /// is a tick that silently authorises the *next* meeting.
    fn withdraw_prompt(&mut self) {
        self.prompt = None;
        self.acknowledged = false;
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
            MenuAction::ToggleRecording => self.toggle(now, StartOrigin::Menu),
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
    pub fn toggle(&mut self, now: Monotonic, origin: StartOrigin) -> Vec<ShellEffect> {
        match self.phase {
            Phase::Idle | Phase::Finished { .. } | Phase::Faulted { .. } => {
                self.handle(ShellInput::Start { at: now, origin })
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
            prompt: self.prompt_view(),
        }
    }

    // --- transitions -----------------------------------------------------

    fn next_phase(&self, input: ShellInput) -> Phase {
        match (&self.phase, input) {
            // Start. Idempotent while a session is live: a second click must
            // not restart the clock the user is watching.
            (
                Phase::Idle | Phase::Finished { .. } | Phase::Faulted { .. },
                ShellInput::Start { at, .. },
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

    fn transition(&mut self, next: Phase, origin: Option<StartOrigin>) -> Vec<ShellEffect> {
        self.phase = next;
        let mut effects = Vec::new();

        // A prompt is a question about starting a recording. Once anything
        // else is happening it is stale, so it comes down.
        if !self.phase.is_idle() {
            self.withdraw_prompt();
        }

        // Capture edges.
        let want_capture = self.phase.is_recording();
        if want_capture && !self.capture {
            self.capture = true;
            // CON-01: the audit record is written before the first byte is
            // captured, and it names the person who asked. Reaching this edge
            // without an origin would mean some input other than `Start` had
            // begun a recording, which is the defect this whole file is
            // arranged to prevent -- `tests/con01_detection_arms_only.rs`
            // fails on a `StartCapture` with no `AuditStart` in front of it.
            debug_assert!(
                origin.is_some(),
                "capture started with no human origin (CON-01)"
            );
            if let Some(origin) = origin {
                effects.push(ShellEffect::AuditStart(origin));
            }
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

    fn prompt_view(&self) -> Option<PromptView> {
        let meeting = self.prompt.as_ref()?;
        Some(PromptView {
            app_key: meeting.app_key.clone(),
            headline: meeting.headline(),
            evidence: meeting.evidence.clone(),
            consent_notice: meeting.consent_notice.clone(),
            requires_acknowledgement: meeting.requires_acknowledgement,
            acknowledged: self.acknowledged,
            // The one rule the renderer must not restate. A panel that
            // computed this itself could disagree with `answer_prompt` above,
            // and the disagreement is invisible: a Start button that looks
            // live and does nothing, or one that looks live and records.
            start_enabled: !meeting.requires_acknowledgement || self.acknowledged,
            // "Start recording" rather than "Yes": the button says what it
            // does, because this is the click that begins recording other
            // people.
            start_label: "Start recording",
            not_now_label: "Not now",
            never_label: "Never for this app",
            acknowledge_label: "Everyone on this call has agreed to be recorded",
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
