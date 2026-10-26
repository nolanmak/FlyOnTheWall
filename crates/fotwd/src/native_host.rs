//! Recorder-driven native interaction model; platform APIs stay in fotw-shell.
use crate::native_bridge::{BridgeSnapshot, RecorderBridge, StartTicket};
use crate::native_shell::RecorderView;
use crate::recording::StopTarget;
use fotw_shell::{
    HotkeyAction, HotkeyMap, Level, MenuAction, Monotonic, PillView, PromptChoice, PromptView,
    ShellView, StartOrigin, SurfaceEvent, SurfaceHost, Tone, TrayState, TrayView,
};
use fotw_web::{RecordingState, RecordingStatus};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

/// Background work requested by a native menu. Never runs on the AppKit thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAction {
    /// Open a fresh authorized dashboard tab.
    OpenDashboard,
    /// Open the dashboard's settings.
    OpenSettings,
    /// Open the disclosure surface.
    OpenDisclosure,
    /// Open application information.
    OpenAbout,
    /// Stop/drain and finish app shutdown.
    Quit,
}
/// Native controls driven by the same recorder as the dashboard.
pub struct NativeHost {
    bridge: RecorderBridge,
    hotkeys: HotkeyMap,
    consent_notice: String,
    actions: SyncSender<NativeAction>,
    snapshot: Option<BridgeSnapshot>,
    ticket: Option<StartTicket>,
    prompt_origin: Option<StartOrigin>,
    acknowledged: bool,
    action_error: Option<String>,
    stop_queued: Option<StopTarget>,
    cancel_requested: bool,
}
impl NativeHost {
    /// Construct after daemon initialization, without starting any capture.
    #[must_use]
    pub fn new(
        bridge: RecorderBridge,
        hotkeys: HotkeyMap,
        consent_notice: String,
        actions: SyncSender<NativeAction>,
    ) -> Self {
        Self {
            bridge,
            hotkeys,
            consent_notice,
            actions,
            snapshot: None,
            ticket: None,
            prompt_origin: None,
            acknowledged: false,
            action_error: None,
            stop_queued: None,
            cancel_requested: false,
        }
    }
    fn toggle(&mut self, origin: StartOrigin) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|s| s.observation.status().is_active())
            || self.ticket.is_some()
        {
            self.stop();
        } else if self.snapshot.is_some() {
            self.prompt_origin = Some(origin);
            self.acknowledged = false;
            self.action_error = None;
        }
    }
    fn stop(&mut self) {
        // Tickets and observed Stop capabilities both retain their own attempt
        // identity. A queued action is never retargeted to a later snapshot.
        if let Some(ticket) = &self.ticket {
            ticket.cancel();
            self.cancel_requested = true;
        }
        if let Some(target) = self
            .snapshot
            .as_ref()
            .and_then(|s| s.observation.stop_target())
            && !self
                .stop_queued
                .as_ref()
                .is_some_and(|queued| queued.same_attempt(target))
        {
            match self.bridge.stop(target) {
                Ok(()) => self.stop_queued = Some(target.clone()),
                Err(error) => self.action_error = Some(error),
            }
        }
    }
    fn request(&mut self, action: NativeAction) {
        if self.actions.try_send(action).is_err() {
            self.action_error = Some("application action could not be queued; try again".into());
        }
    }
    fn prompt(&self) -> Option<PromptView> {
        self.prompt_origin.map(|_| PromptView {
            app_key: "manual-recording".into(),
            headline: "Start recording this meeting?".into(),
            evidence: "Recording captures computer audio and the microphone when available.".into(),
            consent_notice: self.consent_notice.clone(),
            requires_acknowledgement: true,
            acknowledged: self.acknowledged,
            start_enabled: self.acknowledged,
            start_label: "Start recording",
            not_now_label: "Cancel",
            never_label: "",
            acknowledge_label: "Everyone in this meeting has agreed to be recorded",
        })
    }
}
impl SurfaceHost for NativeHost {
    fn on_event(&mut self, event: SurfaceEvent, _now: Monotonic) {
        match event {
            SurfaceEvent::Chord(chord) => match self.hotkeys.action_for(chord) {
                Some(HotkeyAction::ToggleRecording) => self.toggle(StartOrigin::Hotkey),
                Some(HotkeyAction::OpenNotes) => self.request(NativeAction::OpenDashboard),
                None => {}
            },
            SurfaceEvent::Menu(MenuAction::ToggleRecording) => self.toggle(StartOrigin::Menu),
            SurfaceEvent::Menu(action) => {
                let request = match action {
                    MenuAction::OpenNotes => NativeAction::OpenDashboard,
                    MenuAction::Settings => NativeAction::OpenSettings,
                    MenuAction::DisclosureKit => NativeAction::OpenDisclosure,
                    MenuAction::About => NativeAction::OpenAbout,
                    MenuAction::Quit => NativeAction::Quit,
                    MenuAction::ToggleRecording => unreachable!(),
                };
                if request == NativeAction::Quit {
                    self.stop();
                }
                self.request(request);
            }
            SurfaceEvent::Stop => self.stop(),
            SurfaceEvent::Acknowledge(acknowledged) => {
                if self.prompt_origin.is_some() {
                    self.acknowledged = acknowledged;
                }
            }
            SurfaceEvent::Prompt(PromptChoice::Start { acknowledged }) => {
                if self.acknowledged
                    && acknowledged
                    && let Some(origin) = self.prompt_origin.take()
                {
                    self.acknowledged = false;
                    match self.bridge.start(origin, true) {
                        Ok(ticket) => {
                            self.ticket = Some(ticket);
                            self.cancel_requested = false;
                            self.action_error = None;
                        }
                        Err(error) => self.action_error = Some(error),
                    }
                }
            }
            SurfaceEvent::Prompt(PromptChoice::NotNow | PromptChoice::NeverForThisApp) => {
                self.prompt_origin = None;
                self.acknowledged = false;
            }
        }
    }
    fn tick(&mut self, _now: Monotonic) {
        if let Some(snapshot) = self.bridge.poll() {
            if self
                .snapshot
                .as_ref()
                .is_some_and(|previous| previous.sequence >= snapshot.sequence)
            {
                return;
            }
            if let Some(target) = snapshot.observation.stop_target()
                && !self
                    .snapshot
                    .as_ref()
                    .and_then(|previous| previous.observation.stop_target())
                    .is_some_and(|previous| previous.same_attempt(target))
            {
                self.action_error = None;
                self.stop_queued = None;
            }
            if let Some(outcome) = &snapshot.start_outcome
                && self
                    .ticket
                    .as_ref()
                    .is_some_and(|ticket| ticket.id() == outcome.id)
            {
                self.ticket = None;
                let cancelled = self.cancel_requested;
                self.cancel_requested = false;
                // A late Start result never changes the observed recorder
                // phase or covers a different active capture with failure.
                if snapshot.observation.status().state == RecordingState::Idle {
                    self.action_error = if cancelled {
                        None
                    } else {
                        outcome.error.clone()
                    };
                }
            }
            if let Some(error) = &snapshot.command_error {
                self.action_error = Some(error.clone());
                self.stop_queued = None;
            }
            if !snapshot.observation.stop_target().is_some_and(|target| {
                self.stop_queued
                    .as_ref()
                    .is_some_and(|queued| queued.same_attempt(target))
            }) {
                self.stop_queued = None;
            }
            if snapshot.observation.status().is_active() {
                self.prompt_origin = None;
                self.acknowledged = false;
            }
            self.snapshot = Some(snapshot);
        }
    }
    fn view(&self) -> ShellView {
        let status = self
            .snapshot
            .as_ref()
            .map_or_else(RecordingStatus::idle, |s| s.observation.status().clone());
        let observed = RecorderView::from_status(&status);
        let pending = self.ticket.is_some() && !status.is_active();
        let error = observed.error.as_ref().or(self.action_error.as_ref());
        let title = if self.snapshot.is_none() {
            "Connecting"
        } else if pending {
            if self.cancel_requested {
                "Stopping"
            } else {
                "Preparing"
            }
        } else if status.state == RecordingState::Idle && error.is_some() {
            "Needs attention"
        } else {
            observed.title
        };
        let stop_enabled = (observed.stop_enabled || pending)
            && self.stop_queued.is_none()
            && !self.cancel_requested;
        let mut detail = observed.detail;
        if let Some(error) = error {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str(error);
        }
        let phase = match status.state {
            RecordingState::Recording => TrayState::Recording,
            RecordingState::Finishing => TrayState::Finishing,
            RecordingState::Preparing | RecordingState::Starting => TrayState::Preparing,
            RecordingState::Idle if pending => TrayState::Preparing,
            RecordingState::Idle if error.is_some() => TrayState::Fault,
            RecordingState::Idle => TrayState::Idle,
        };
        let tone = match phase {
            TrayState::Recording => Tone::Live,
            TrayState::Finishing => Tone::Finishing,
            TrayState::Fault => Tone::Fault,
            TrayState::Preparing | TrayState::Idle => Tone::Preparing,
        };
        let elapsed = observed.elapsed_label;
        let status_label = elapsed.as_ref().map_or_else(
            || title.to_owned(),
            |elapsed| format!("{title} — {elapsed}"),
        );
        let mut menu = fotw_shell::ShellCore::new().view().menu;
        menu.status.clone_from(&status_label);
        menu.record.label = if pending || status.is_active() {
            "Stop Recording"
        } else {
            "Start Recording…"
        }
        .into();
        menu.record.enabled = self.snapshot.is_some()
            && if pending || status.is_active() {
                stop_enabled
            } else {
                true
            };
        ShellView {
            pill: (status.is_active() || pending || error.is_some()).then(|| PillView {
                elapsed: Duration::from_millis(status.elapsed_ms.unwrap_or(0)),
                elapsed_label: elapsed.clone().unwrap_or_else(|| "—".into()),
                status_label: title,
                level: Level::SILENT,
                level_available: false,
                detail: detail.clone(),
                tone,
                stop_enabled,
            }),
            tray: TrayView {
                state: phase,
                title: if status.is_active() || pending {
                    Some(elapsed.unwrap_or_else(|| title.into()))
                } else {
                    None
                },
                tooltip: if detail.is_empty() {
                    status_label
                } else {
                    format!("{status_label}. {detail}")
                },
            },
            menu,
            prompt: self.prompt(),
        }
    }
    fn did_render(&mut self) {
        if self.view().pill.is_some()
            && let Some(target) = self
                .snapshot
                .as_ref()
                .and_then(|s| s.observation.stop_target())
        {
            self.bridge.did_render(target);
        }
    }
}
