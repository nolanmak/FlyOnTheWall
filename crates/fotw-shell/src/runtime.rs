//! The seam between the state machine and whatever is actually on screen.
//!
//! [`ShellRuntime`] owns a [`ShellCore`] and a [`HotkeyMap`] and routes
//! [`ShellEffect`]s to a [`ShellHost`]. It contains no platform types, so the
//! whole event-to-effect-to-host path — the part that would otherwise only be
//! exercisable by clicking a menu bar — runs headless in CI against a fake
//! host.
//!
//! The AppKit layer's entire job is then: pump the OS event sources into
//! [`ShellRuntime::on_chord`] / [`ShellRuntime::on_menu`] /
//! [`ShellRuntime::tick`], and paint [`ShellRuntime::view`].

use std::collections::VecDeque;

use crate::clock::Monotonic;
use crate::hotkey::{Chord, HotkeyMap};
use crate::prompt::{DetectedMeeting, DetectionUpdate, PromptChoice, StartOrigin};
use crate::state::{ShellCore, ShellEffect, ShellInput};
use crate::view::{Level, MenuAction, ShellView};

/// What the shell asks the rest of the application to do.
///
/// Everything except [`ShellHost::start_capture`], [`ShellHost::stop_capture`]
/// and [`ShellHost::quit`] has a no-op default, so a caller that only wants a
/// recording indicator can implement three methods.
pub trait ShellHost {
    /// Begin capturing. The `Err` string is shown to the user as the fault
    /// reason, so it must be something a person can act on.
    ///
    /// # Errors
    ///
    /// Whatever the capture layer reports.
    fn start_capture(&mut self) -> Result<(), String>;

    /// Tear the capture down. Called for the Stop button and for a fault, and
    /// must be safe to call when nothing is running.
    fn stop_capture(&mut self);

    /// Record a user-initiated Start in the local audit log (CON-01, CON-08).
    ///
    /// Called immediately before [`ShellHost::start_capture`]. Deliberately
    /// **not** given a default no-op implementation: CON-01's acceptance
    /// criterion is that a fresh install never writes an audio buffer without
    /// a user-initiated Start event *in the audit log*, and a host that
    /// silently inherited an empty implementation would satisfy the type
    /// system while failing the requirement.
    fn audit_start(&mut self, origin: StartOrigin);

    /// Quit the application.
    fn quit(&mut self);

    /// The current input level, polled once per tick while recording.
    fn level(&mut self) -> Level {
        Level::SILENT
    }

    /// Start or stop delivering ticks.
    ///
    /// The shell only needs a timer while something is on screen. A renderer
    /// that ignores this and polls unconditionally is still correct, just
    /// marginally wasteful.
    fn set_ticking(&mut self, _ticking: bool) {}

    /// Bring the notes window forward.
    fn open_notes(&mut self) {}

    /// Open the Disclosure Kit (CON-03).
    fn open_disclosure_kit(&mut self) {}

    /// Open settings.
    fn open_settings(&mut self) {}

    /// Open the about box.
    fn open_about(&mut self) {}

    /// The user said "not now" to a detection prompt: back the detector off.
    ///
    /// A no-op default is safe here in a way it is not for
    /// [`ShellHost::audit_start`]: ignoring it costs a repeated prompt, not a
    /// missing consent record.
    fn snooze_detection(&mut self) {}

    /// The user said "never for this app": persist that suppression.
    fn suppress_app(&mut self, _app_key: &str) {}

    /// Anything the host's meeting detector has to say, polled once per tick.
    ///
    /// The default is `None`: a host with no detector never arms anything,
    /// which is the correct behaviour and not a degraded one.
    ///
    /// **This is the only way a detection reaches the screen.** It is
    /// deliberately a poll rather than a channel of arbitrary
    /// [`ShellInput`]s: the host may say "a meeting appears to be happening"
    /// and "it stopped", and nothing else. A host that could push a
    /// [`ShellInput::Start`] would be a host that could record without a
    /// person, which is the whole of CON-01.
    fn poll_detection(&mut self) -> Option<DetectionUpdate> {
        None
    }
}

/// A [`ShellCore`] wired to a [`ShellHost`].
#[derive(Debug)]
pub struct ShellRuntime<H: ShellHost> {
    core: ShellCore,
    hotkeys: HotkeyMap,
    host: H,
}

impl<H: ShellHost> ShellRuntime<H> {
    /// A runtime with the shipped hotkeys.
    pub fn new(host: H) -> Self {
        Self::with_hotkeys(host, HotkeyMap::defaults())
    }

    /// A runtime with a specific hotkey table.
    pub const fn with_hotkeys(host: H, hotkeys: HotkeyMap) -> Self {
        Self {
            core: ShellCore::new(),
            hotkeys,
            host,
        }
    }

    /// The state machine.
    #[must_use]
    pub const fn core(&self) -> &ShellCore {
        &self.core
    }

    /// The hotkey table.
    #[must_use]
    pub const fn hotkeys(&self) -> &HotkeyMap {
        &self.hotkeys
    }

    /// The host.
    #[must_use]
    pub const fn host(&self) -> &H {
        &self.host
    }

    /// The host, mutably.
    pub const fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// What to paint.
    #[must_use]
    pub fn view(&self) -> ShellView {
        self.core.view()
    }

    /// Advance the clock. Samples the host's level first so the meter and the
    /// elapsed label describe the same instant.
    ///
    /// Also drains the host's detector, which is what puts a prompt on screen
    /// in a running shell (CON-01, issue #52).
    pub fn tick(&mut self, now: Monotonic) {
        match self.host.poll_detection() {
            Some(DetectionUpdate::Armed(meeting)) => self.meeting_detected(now, meeting),
            Some(DetectionUpdate::Cleared) => self.detection_cleared(),
            None => {}
        }

        if self.core.phase().is_recording() {
            let level = self.host.level();
            let effects = self.core.handle(ShellInput::Level(level));
            self.dispatch(effects);
        }
        let effects = self.core.handle(ShellInput::Tick { now });
        self.dispatch(effects);
    }

    /// Handle a global hotkey. Returns whether the chord was bound.
    pub fn on_chord(&mut self, chord: Chord, now: Monotonic) -> bool {
        let Some(action) = self.hotkeys.action_for(chord) else {
            return false;
        };
        let effects = match action {
            crate::hotkey::HotkeyAction::ToggleRecording => {
                self.core.toggle(now, StartOrigin::Hotkey)
            }
            crate::hotkey::HotkeyAction::OpenNotes => vec![ShellEffect::OpenNotes],
        };
        self.dispatch(effects);
        true
    }

    /// The detector believes a meeting is in progress.
    ///
    /// **Arms only.** This raises a prompt and returns; nothing here can
    /// begin a capture, and `tests/con01_detection_arms_only.rs` exists to
    /// keep it that way (CON-01, issue #22).
    pub fn meeting_detected(&mut self, at: Monotonic, meeting: DetectedMeeting) {
        let effects = self
            .core
            .handle(ShellInput::MeetingDetected { at, meeting });
        self.dispatch(effects);
    }

    /// The detector's signals went away before the user answered.
    pub fn detection_cleared(&mut self) {
        let effects = self.core.handle(ShellInput::DetectionCleared);
        self.dispatch(effects);
    }

    /// The user ticked or cleared the prompt's all-party acknowledgement.
    ///
    /// Produces no effects and reaches no host method: it is a checkbox. The
    /// only observable consequence is
    /// [`PromptView::start_enabled`](crate::PromptView::start_enabled).
    pub fn acknowledge_prompt(&mut self, acknowledged: bool) {
        let effects = self
            .core
            .handle(ShellInput::PromptAcknowledged { acknowledged });
        self.dispatch(effects);
    }

    /// The user answered the detection prompt.
    pub fn respond_to_prompt(&mut self, choice: PromptChoice, now: Monotonic) {
        let effects = self
            .core
            .handle(ShellInput::PromptResponse { at: now, choice });
        self.dispatch(effects);
    }

    /// The user pressed Start somewhere that is not the menu or the hotkey —
    /// the browser UI on loopback.
    pub fn request_start(&mut self, origin: StartOrigin, now: Monotonic) {
        let effects = self.core.handle(ShellInput::Start { at: now, origin });
        self.dispatch(effects);
    }

    /// Handle a menu click.
    pub fn on_menu(&mut self, action: MenuAction, now: Monotonic) {
        let effects = self.core.on_menu(action, now);
        self.dispatch(effects);
    }

    /// The user pressed Stop — on the pill, or anywhere else that offers it.
    pub fn request_stop(&mut self) {
        let effects = self.core.handle(ShellInput::StopRequested);
        self.dispatch(effects);
    }

    /// The capture layer reports the session is written and closed.
    pub fn capture_finished(&mut self, at: Monotonic) {
        let effects = self.core.handle(ShellInput::StopCompleted { at });
        self.dispatch(effects);
    }

    /// The capture layer reports a failure.
    pub fn capture_failed(&mut self, reason: impl Into<String>) {
        let effects = self.core.handle(ShellInput::CaptureFailed {
            reason: reason.into(),
        });
        self.dispatch(effects);
    }

    /// The user acknowledged a finished or failed session.
    pub fn dismiss(&mut self) {
        let effects = self.core.handle(ShellInput::Dismiss);
        self.dispatch(effects);
    }

    fn dispatch(&mut self, effects: Vec<ShellEffect>) {
        let mut queue: VecDeque<ShellEffect> = effects.into();
        // A failed `start_capture` feeds `CaptureFailed` back in, which emits
        // `StopCapture` and nothing further, so the real depth is two. The
        // counter is a guard against a future edit introducing a cycle -- it
        // drops work rather than spinning or panicking, because a shell that
        // aborts is strictly worse than one with a stale menu.
        let mut guard = 0usize;
        while let Some(effect) = queue.pop_front() {
            guard += 1;
            if guard > 64 {
                break;
            }
            match effect {
                ShellEffect::AuditStart(origin) => self.host.audit_start(origin),
                ShellEffect::SnoozeDetection => self.host.snooze_detection(),
                ShellEffect::SuppressApp(app_key) => self.host.suppress_app(&app_key),
                ShellEffect::StartCapture => {
                    if let Err(reason) = self.host.start_capture() {
                        queue.extend(self.core.handle(ShellInput::CaptureFailed { reason }));
                    }
                }
                ShellEffect::StopCapture => self.host.stop_capture(),
                ShellEffect::StartTicking => self.host.set_ticking(true),
                ShellEffect::StopTicking => self.host.set_ticking(false),
                ShellEffect::OpenNotes => self.host.open_notes(),
                ShellEffect::OpenDisclosureKit => self.host.open_disclosure_kit(),
                ShellEffect::OpenSettings => self.host.open_settings(),
                ShellEffect::OpenAbout => self.host.open_about(),
                ShellEffect::Quit => self.host.quit(),
            }
        }
    }
}
