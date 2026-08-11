//! What the shell shows — as plain data, with no AppKit in any signature.
//!
//! [`ShellCore::view`](crate::ShellCore::view) is a pure function of the
//! state machine, and the AppKit layer is a renderer that does nothing but
//! apply the result. That split is what makes any of this testable on a CI
//! runner with no window server (docs/REQUIREMENTS.md 5.6).
//!
//! # CON-02 lives in the shape of these types
//!
//! [`MenuModel`] is a fixed-shape struct rather than a list of entries the
//! core assembles. Two reasons, one cosmetic and one legal:
//!
//! - The renderer can build the `NSMenu` once and only ever call `set_text` /
//!   `set_enabled`, so the menu never rebuilds under an open cursor.
//! - There is no code path that *omits* a row. A build that suppressed the
//!   recording indicator would have to delete a field, which is a visible
//!   diff, not a flag. See docs/REQUIREMENTS.md 11.2 CON-02.

use std::time::Duration;

/// A normalised audio level, clamped to `0.0..=1.0`.
///
/// The pill shows one for the same reason CON-02 requires elapsed time: a
/// participant glancing at the screen must be able to tell that the thing is
/// actually capturing, not merely displaying a badge.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Level(f32);

impl Level {
    /// No signal.
    pub const SILENT: Self = Self(0.0);

    /// Clamp an arbitrary float into a level.
    ///
    /// NaN becomes [`Level::SILENT`]: an RMS calculation over a buffer of
    /// denormals can produce one, and a NaN reaching the meter would render
    /// as an empty bar forever with no other symptom.
    #[must_use]
    pub fn new(value: f32) -> Self {
        if value.is_nan() {
            return Self::SILENT;
        }
        Self(value.clamp(0.0, 1.0))
    }

    /// The clamped value.
    #[must_use]
    pub fn get(self) -> f32 {
        self.0
    }

    /// How many of `total` meter segments are lit.
    ///
    /// Any audible signal lights at least one segment. A meter that reads
    /// empty during quiet speech is indistinguishable from a dead tap, which
    /// is the failure this display exists to make visible.
    #[must_use]
    pub fn bars(self, total: usize) -> usize {
        if total == 0 || self.0 <= 0.0 {
            return 0;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "self.0 is clamped to 0..=1 and total is a segment count"
        )]
        let lit = (self.0 * total as f32).ceil() as usize;
        lit.clamp(1, total)
    }

    /// The meter as text, e.g. `▮▮▮▯▯`.
    ///
    /// A glyph meter rather than a drawn one: it needs no `drawRect:`
    /// override, no layer-backed view and no redraw scheduling, and it is
    /// exactly as legible at pill size. Replacing it with a drawn meter is a
    /// change to the renderer alone.
    #[must_use]
    pub fn meter(self, total: usize) -> String {
        let lit = self.bars(total);
        let mut out = String::with_capacity(total * 3);
        for i in 0..total {
            out.push(if i < lit { '▮' } else { '▯' });
        }
        out
    }
}

/// The visual register of the pill and the menu-bar item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Capture is live.
    Live,
    /// Capture has stopped but the session is still being written to disk.
    Finishing,
    /// The session is written and closed.
    Saved,
    /// Capture failed. Requires an explicit acknowledgement.
    Fault,
}

/// The always-on-top recording pill (CON-02).
///
/// The core hands this out as `Some` for the whole time capture is live *and*
/// for the flush and acknowledgement windows on either side. The renderer has
/// no other way to learn whether the pill should be on screen, and no API of
/// its own to take it down.
#[derive(Clone, Debug, PartialEq)]
pub struct PillView {
    /// Wall time since the session started.
    pub elapsed: Duration,
    /// [`elapsed`](Self::elapsed) rendered, e.g. `12:34` or `1:02:03`.
    pub elapsed_label: String,
    /// Short state word, e.g. `Recording`.
    pub status_label: &'static str,
    /// Current input level.
    pub level: Level,
    /// Visual register.
    pub tone: Tone,
    /// Whether the Stop button accepts a click.
    ///
    /// False while a stop is already in flight, so a second click cannot
    /// re-enter the teardown path.
    pub stop_enabled: bool,
}

impl PillView {
    /// The whole pill as one line, the way a screenshot would read it.
    ///
    /// Used by the renderer for the accessibility label and by tests as a
    /// single assertion over the rendered state.
    #[must_use]
    pub fn accessibility_label(&self) -> String {
        format!("{} — {}", self.status_label, self.elapsed_label)
    }
}

/// The menu-bar item's state. Idle and recording must be visually distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayState {
    /// Not recording.
    Idle,
    /// Capture is live.
    Recording,
    /// Capture stopped, session still flushing.
    Finishing,
    /// Capture failed.
    Fault,
}

/// The menu-bar item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayView {
    /// Which icon to show.
    pub state: TrayState,
    /// Text drawn beside the icon in the menu bar.
    ///
    /// `Some(elapsed)` while a session is live or flushing. This is a second,
    /// independent surface for CON-02: the elapsed clock is legible even if
    /// the pill is behind a full-screen space that has not yet been composited.
    pub title: Option<String>,
    /// Hover tooltip.
    pub tooltip: String,
}

/// One clickable row of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuButton {
    /// Row text.
    pub label: String,
    /// Whether the row accepts a click.
    pub enabled: bool,
}

impl MenuButton {
    fn new(label: impl Into<String>, enabled: bool) -> Self {
        Self {
            label: label.into(),
            enabled,
        }
    }
}

/// Everything the status-item menu offers.
///
/// Fixed shape on purpose — see the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuModel {
    /// Disabled informational first row, e.g. `Recording — 12:34`.
    pub status: String,
    /// Start or stop, depending on phase. Dispatches [`MenuAction::ToggleRecording`].
    pub record: MenuButton,
    /// Open the notes window.
    pub notes: MenuButton,
    /// Open the Disclosure Kit (CON-03).
    pub disclosure: MenuButton,
    /// Open settings.
    pub settings: MenuButton,
    /// About box.
    pub about: MenuButton,
    /// Quit.
    pub quit: MenuButton,
}

impl MenuModel {
    pub(crate) fn build(record: MenuButton, status: String) -> Self {
        Self {
            status,
            record,
            notes: MenuButton::new("Open Notes", true),
            disclosure: MenuButton::new("Disclosure Kit…", true),
            settings: MenuButton::new("Settings…", true),
            about: MenuButton::new("About FlyOnTheWall", true),
            // Deliberately always enabled. Quitting stops the recording, which
            // is a legitimate way to end a session; refusing to quit while
            // recording traps the user in the state they are trying to leave.
            quit: MenuButton::new("Quit FlyOnTheWall", true),
        }
    }

    /// The row a given action would activate.
    #[must_use]
    pub fn button(&self, action: MenuAction) -> &MenuButton {
        match action {
            MenuAction::ToggleRecording => &self.record,
            MenuAction::OpenNotes => &self.notes,
            MenuAction::DisclosureKit => &self.disclosure,
            MenuAction::Settings => &self.settings,
            MenuAction::About => &self.about,
            MenuAction::Quit => &self.quit,
        }
    }
}

/// Every menu row that does something.
///
/// Exhaustively matched by the renderer and by
/// [`MenuModel::button`], so a new row cannot be added without deciding what
/// it looks like in every phase. There is deliberately no variant that hides
/// the indicator (docs/REQUIREMENTS.md 11.2 CON-02).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MenuAction {
    /// Start a session, or stop the running one.
    ToggleRecording,
    /// Open the notes window.
    OpenNotes,
    /// Open the Disclosure Kit.
    DisclosureKit,
    /// Open settings.
    Settings,
    /// Open the about box.
    About,
    /// Quit the app.
    Quit,
}

impl MenuAction {
    /// Every action, in menu order. Used by the renderer to wire the `NSMenu`.
    pub const ALL: [Self; 6] = [
        Self::ToggleRecording,
        Self::OpenNotes,
        Self::DisclosureKit,
        Self::Settings,
        Self::About,
        Self::Quit,
    ];

    /// A stable identifier for the row, used as the `muda::MenuId`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::ToggleRecording => "fotw.record",
            Self::OpenNotes => "fotw.notes",
            Self::DisclosureKit => "fotw.disclosure",
            Self::Settings => "fotw.settings",
            Self::About => "fotw.about",
            Self::Quit => "fotw.quit",
        }
    }

    /// Recover an action from its [`id`](Self::id).
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }
}

/// Everything on screen, for one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct ShellView {
    /// The recording pill, or `None` when there is nothing to indicate.
    pub pill: Option<PillView>,
    /// The menu-bar item.
    pub tray: TrayView,
    /// The status-item menu.
    pub menu: MenuModel,
}
