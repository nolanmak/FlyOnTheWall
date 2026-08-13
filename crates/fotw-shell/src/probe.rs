//! A startup self-check for the three shell surfaces.
//!
//! Built for `fotw doctor` (docs/REQUIREMENTS.md 5.6), and it is also the only
//! way to observe the property this crate is built around: whether the
//! non-activating style mask actually survived `NSPanel` initialization.
//!
//! A unit test can assert that the *constant* contains
//! `NSWindowStyleMask::NonactivatingPanel`. Only a running window server can
//! say whether AppKit kept it — which is the whole question, since
//! `setStyleMask:` accepts the bit and then does nothing with it
//! (FB16484811). [`ShellProbe::panel_style_mask`] is read back **from the
//! panel** after construction, not from the constant that went in.

/// What came up, and what AppKit says about it afterwards.
///
/// Every field is read back from the live object rather than echoed from the
/// value that was set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a diagnostic report of independent yes/no checks; collapsing them \
              into a bitflag or an enum would make the printed output worse and \
              hide which specific property regressed"
)]
pub struct ShellProbe {
    /// `NSApp.activationPolicy` — 1 is `Accessory`.
    pub activation_policy: isize,
    /// The style mask `NSPanel` reports **after** initialization.
    pub panel_style_mask: usize,
    /// Whether that mask still contains `NonactivatingPanel` (1<<7).
    ///
    /// If this is false the pill will steal focus from the meeting app on
    /// every click, and nothing else in the shell will look wrong.
    pub panel_is_nonactivating: bool,
    /// What the panel answers to `canBecomeKeyWindow`.
    ///
    /// Must be true, or the Stop button swallows the first click.
    pub panel_can_become_key: bool,
    /// What the panel answers to `canBecomeMainWindow`. Must be false.
    pub panel_can_become_main: bool,
    /// `NSWindow.level` — must be 25 (`NSStatusWindowLevel`), not 3.
    pub panel_level: isize,
    /// `NSWindow.sharingType` — must be 0 (`NSWindowSharingNone`).
    pub panel_sharing_type: usize,
    /// `NSWindow.collectionBehavior`.
    pub panel_collection_behavior: usize,

    /// The detection prompt's style mask, after initialization.
    pub prompt_style_mask: usize,
    /// Whether the prompt kept `NonactivatingPanel`.
    ///
    /// If this is false the prompt takes focus off the meeting the instant it
    /// appears — while the user is mid-sentence in the call it is asking
    /// about.
    pub prompt_is_nonactivating: bool,
    /// What the prompt answers to `canBecomeKeyWindow`. Must be true, or the
    /// first click on Start is swallowed.
    pub prompt_can_become_key: bool,
    /// What the prompt answers to `canBecomeMainWindow`. Must be false.
    pub prompt_can_become_main: bool,
    /// The prompt's `NSWindow.level` — must be 25, not 3.
    pub prompt_level: isize,
    /// The prompt's `NSWindow.sharingType` — must be 0.
    pub prompt_sharing_type: usize,
    /// The prompt's `NSWindow.collectionBehavior`.
    pub prompt_collection_behavior: usize,
    /// Whether a blocking (all-party) prompt drew its Start button
    /// **disabled**, read off the live `NSButton` (CON-05).
    ///
    /// This is the only evidence that the renderer applied the core's gate
    /// rather than merely receiving it. A unit test can prove
    /// `PromptView::start_enabled` is false; only the button knows whether
    /// anybody used it.
    pub prompt_blocking_start_disabled: bool,
    /// Whether that same button became enabled once the acknowledgement was
    /// ticked. False here means a prompt nobody can ever start — the failure
    /// that looks like "detection is broken".
    pub prompt_acknowledged_start_enabled: bool,
    /// Whether every rendered control fits inside the panel, measured against
    /// the live text at the live font size.
    ///
    /// False means the jurisdiction warning is clipped, which is worse than
    /// showing none at all: the user reads half a sentence about criminal
    /// liability and presses Start.
    pub prompt_content_fits: bool,
    /// Whether the panel came up **on** a screen, rather than being ordered
    /// front at a position no display covers.
    pub prompt_is_on_a_screen: bool,
    /// Whether a real `performClick:` on the disabled Start button produced
    /// no response at all (CON-05, as behaviour rather than as a flag).
    pub prompt_disabled_start_swallows_the_click: bool,
    /// Whether the acknowledgement checkbox's target/action actually
    /// dispatched. A mistyped selector compiles and then throws
    /// `unrecognized selector` under the user's finger.
    pub prompt_checkbox_dispatches: bool,
    /// Whether a real click on the enabled Start button came back as a
    /// `PromptChoice::Start` carrying the acknowledgement.
    ///
    /// This is the end of the only path that can begin a recording from a
    /// detection prompt, exercised through AppKit rather than around it.
    pub prompt_start_click_dispatches: bool,
    /// Whether the other two answers dispatch as themselves — a prompt whose
    /// "Never for this app" silently starts a recording is the worst possible
    /// mis-wiring, and swapped selectors are how it would happen.
    pub prompt_dismissals_dispatch: bool,

    /// Whether `tray-icon` handed back a retained `NSStatusItem`.
    pub status_item_retained: bool,
    /// How many global hotkeys the OS accepted.
    pub hotkeys_registered: usize,
}

impl ShellProbe {
    /// Whether every property the shell depends on came back correct.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.activation_policy == 1
            && self.panel_is_nonactivating
            && self.panel_can_become_key
            && !self.panel_can_become_main
            && self.panel_level == 25
            && self.panel_sharing_type == 0
            && self.prompt_is_nonactivating
            && self.prompt_can_become_key
            && !self.prompt_can_become_main
            && self.prompt_level == 25
            && self.prompt_sharing_type == 0
            && self.prompt_blocking_start_disabled
            && self.prompt_acknowledged_start_enabled
            && self.prompt_content_fits
            && self.prompt_is_on_a_screen
            && self.prompt_disabled_start_swallows_the_click
            && self.prompt_checkbox_dispatches
            && self.prompt_start_click_dispatches
            && self.prompt_dismissals_dispatch
            && self.status_item_retained
    }
}
