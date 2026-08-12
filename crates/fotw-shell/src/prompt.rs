//! Arming: what detection is allowed to do, and what only a person may do.
//!
//! Meeting detection raises a prompt. **It does not begin capture.** That is
//! CON-01 (docs/REQUIREMENTS.md 11.2) and issue #22, and it is a legal
//! position rather than a UX one: starting a recording from a signal instead
//! of from a person is the specific conduct pleaded in *Chamberlain v.
//! Granola* under CIPA § 637.2(a), at $5,000 per recorded participant.
//!
//! The types here are shaped so that the requirement is hard to violate by
//! accident:
//!
//! - [`StartOrigin`] has **no automatic variant**. Every way of starting a
//!   recording has to name the human who asked, so adding a silent
//!   auto-record path means adding a variant to a public enum whose
//!   documentation says not to — a visible diff, not a flag.
//! - [`DetectedMeeting`] carries no method that starts anything. It is
//!   evidence to show a person.
//! - [`PromptChoice::Start`] carries the consent acknowledgement, so the
//!   blocking all-party warning (CON-05) cannot be bypassed by a caller that
//!   simply forgets to ask.

/// Who asked for this recording.
///
/// **Every variant is a human action.** There is deliberately no `Automatic`,
/// `Detected` or `Scheduled` variant: detection arms, a person starts
/// (CON-01). This value is written to the local audit log, which is the
/// artifact CON-01's acceptance criterion is stated against — "a fresh install
/// never writes an audio buffer to disk without a user-initiated Start event
/// in the local audit log".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StartOrigin {
    /// The menu-bar item's Start row.
    Menu,
    /// The global hotkey.
    Hotkey,
    /// The Start button in the browser UI on loopback.
    WebUi,
    /// The Start button on a meeting-detection prompt. Still a human: the
    /// prompt does nothing until it is clicked.
    DetectionPrompt,
    /// Someone typed `fotwd record`. Also a human, and the only origin that
    /// exists before the shell is running.
    Cli,
}

impl StartOrigin {
    /// Every origin. Exhaustively matched by
    /// [`StartOrigin::label`], so a new one cannot be added without deciding
    /// what the audit log calls it.
    pub const ALL: [Self; 5] = [
        Self::Menu,
        Self::Hotkey,
        Self::WebUi,
        Self::DetectionPrompt,
        Self::Cli,
    ];

    /// Whether a person performed this action.
    ///
    /// Constantly true, and that is the point: the test that asserts it over
    /// [`StartOrigin::ALL`] fails the moment someone adds an origin that is
    /// not a person.
    #[must_use]
    pub const fn is_human(self) -> bool {
        match self {
            Self::Menu | Self::Hotkey | Self::WebUi | Self::DetectionPrompt | Self::Cli => true,
        }
    }

    /// The string written to the audit log. Stable — it is a record format.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::Hotkey => "hotkey",
            Self::WebUi => "web-ui",
            Self::DetectionPrompt => "detection-prompt",
            Self::Cli => "cli",
        }
    }
}

/// A meeting the detector believes is happening.
///
/// Deliberately inert. It is a claim to put in front of a person, with the
/// evidence attached so they can tell whether the detector is right — a prompt
/// that says only "meeting detected" trains people to dismiss it, and the
/// all-party consent warning rides on the same surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedMeeting {
    /// Stable key for the app, used by "never detect from this app".
    pub app_key: String,
    /// What to call the app in the prompt, e.g. `Zoom`.
    pub app_name: String,
    /// Why the detector thinks this is a meeting, in one line a person can
    /// judge: *"Zoom is holding the microphone"*.
    pub evidence: String,
    /// The matched calendar event's title, when there is one.
    pub title: Option<String>,
    /// The jurisdiction warning to show (CON-05). Empty when the consent
    /// engine had nothing to say.
    pub consent_notice: String,
    /// Whether that warning is blocking — an all-party or contested
    /// jurisdiction, which requires an explicit acknowledgement before
    /// recording may begin.
    pub requires_acknowledgement: bool,
}

impl DetectedMeeting {
    /// A detection with no calendar match and no consent notice.
    pub fn new(
        app_key: impl Into<String>,
        app_name: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            app_key: app_key.into(),
            app_name: app_name.into(),
            evidence: evidence.into(),
            title: None,
            consent_notice: String::new(),
            requires_acknowledgement: false,
        }
    }

    /// Attach the matched calendar event title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Attach the consent engine's verdict.
    #[must_use]
    pub fn with_consent_notice(mut self, notice: impl Into<String>, blocking: bool) -> Self {
        self.consent_notice = notice.into();
        self.requires_acknowledgement = blocking;
        self
    }

    /// The prompt's first line.
    #[must_use]
    pub fn headline(&self) -> String {
        match &self.title {
            Some(title) => format!("Meeting detected — {title} ({}). Record?", self.app_name),
            None => format!("{} meeting detected. Record?", self.app_name),
        }
    }
}

/// What the person did with the prompt.
///
/// Three answers, matching issue #22: *Start* / *Not now* / *Never for this
/// app*. There is no fourth answer that means "and do it automatically next
/// time" — an auto-start preference is a settings decision with its own
/// confirmation step, not something to slip into a prompt someone is trying
/// to dismiss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptChoice {
    /// Start recording now.
    Start {
        /// Whether the user ticked the all-party consent acknowledgement.
        ///
        /// Ignored unless the prompt
        /// [requires it](DetectedMeeting::requires_acknowledgement); when it
        /// does, a `false` here leaves the prompt on screen and starts
        /// nothing.
        acknowledged: bool,
    },
    /// Not this time. The detector should back off for a while.
    NotNow,
    /// Never prompt for this app again.
    NeverForThisApp,
}
