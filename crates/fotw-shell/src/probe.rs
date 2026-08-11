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
            && self.status_item_retained
    }
}
