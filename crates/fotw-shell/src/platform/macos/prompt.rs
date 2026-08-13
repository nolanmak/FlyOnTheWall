//! The meeting-detection prompt: the panel that makes CON-01 exist for users.
//!
//! `ShellCore` has produced a [`PromptView`] since detection landed, and until
//! this file existed nothing drew it (issue #52). That is not a cosmetic gap.
//! CON-01's design is *detection arms, a person starts* — if the person is
//! never shown the question, detection arms into a void, every test still
//! passes, and the feature does not exist.
//!
//! # Same four traps as the pill, for sharper reasons
//!
//! This panel appears **while the user is in a meeting**, so every trap in
//! [`super::pill`] applies here with a worse failure:
//!
//! - **`NonactivatingPanel` passed to the initializer**, never to
//!   `setStyleMask:` (FB16484811). Getting it wrong means the prompt takes
//!   focus off the meeting the moment it appears — mid-sentence, mid-typing.
//! - **`NSStatusWindowLevel` (25), not floating (3)**, and never
//!   `setFloatingPanel(true)`, which *assigns* the level and silently
//!   overwrites it. A prompt that cannot sit above a full-screen Zoom is a
//!   prompt nobody sees, because full-screen Zoom is when it fires.
//! - **`canBecomeKeyWindow` must answer `YES`** or the first click on Start is
//!   swallowed by the window that is not key.
//! - Every label is `labelWithString:` — deliberately **not**
//!   `wrappingLabelWithString:`, which produces a *selectable* field.
//!   Selectable text needs key focus, and `becomesKeyOnlyIfNeeded` would then
//!   hand it the keyboard: clicking the prompt would stop the user typing into
//!   their call. Wrapping is turned on through the cell instead.
//!
//! # What this file is allowed to decide
//!
//! Nothing about consent. Whether Start is clickable comes from
//! [`PromptView::start_enabled`], which the core computes; the checkbox state
//! comes from [`PromptView::acknowledged`]; the click goes back as a
//! [`PromptChoice`]. This module chooses fonts and rectangles. That split is
//! what `tests/prompt_surface.rs` can test and a panel cannot.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSPanel, NSScreen, NSStatusWindowLevel, NSTextField,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowCollectionBehavior, NSWindowLevel, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::prompt::PromptChoice;
use crate::view::PromptView;

/// Inner padding, and the gap between stacked rows.
const PAD: f64 = 16.0;
const GAP: f64 = 10.0;

/// Narrowest the panel is allowed to be. It grows to fit its buttons.
const MIN_WIDTH: f64 = 380.0;

/// Widest it is allowed to be: past this, a wall of text stops being read.
const MAX_WIDTH: f64 = 520.0;

/// Height of the button row and of the acknowledgement checkbox.
const BUTTON_HEIGHT: f64 = 24.0;
const CHECKBOX_HEIGHT: f64 = 20.0;

/// Gap between buttons in the row.
const BUTTON_GAP: f64 = 8.0;

/// The shortest a single line of 11-point system text can plausibly be.
///
/// Used only as a *floor* by [`Prompt::fits`], never for layout: it is the
/// half of that check which does not depend on AppKit agreeing with itself.
const MIN_LINE_HEIGHT: f64 = 12.0;

/// Distance from the top-right corner of the screen's visible frame.
const SCREEN_MARGIN: f64 = 20.0;

/// **The style mask, assembled here so it can only ever be passed to the
/// initializer.** See the module docs and [`super::pill`].
const PROMPT_STYLE: NSWindowStyleMask = NSWindowStyleMask(
    NSWindowStyleMask::NonactivatingPanel.0
        | NSWindowStyleMask::Borderless.0
        | NSWindowStyleMask::FullSizeContentView.0,
);

/// Above a full-screen meeting window, on every space, out of ⌘-tab.
const PROMPT_BEHAVIOR: NSWindowCollectionBehavior = NSWindowCollectionBehavior(
    NSWindowCollectionBehavior::CanJoinAllSpaces.0
        | NSWindowCollectionBehavior::FullScreenAuxiliary.0
        | NSWindowCollectionBehavior::Stationary.0
        | NSWindowCollectionBehavior::IgnoresCycle.0,
);

/// `NSStatusWindowLevel` (25). Floating (3) is not above a full-screen space.
const PROMPT_LEVEL: NSWindowLevel = NSStatusWindowLevel;

/// Kept out of the user's own screen share, like the pill.
///
/// A prompt naming the meeting is not the recording indicator CON-02 requires
/// to be visible — nothing is recording yet — and broadcasting *"shall I
/// record this?"* into the call is a worse disclosure than none.
const PROMPT_SHARING: NSWindowSharingType = NSWindowSharingType::None;

/// A control [`Prompt::press`] can drive, for the startup probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbePress {
    Start,
    NotNow,
    Never,
    Acknowledge,
}

/// Which button was pressed. Read by the pump, one per poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Click {
    Start,
    NotNow,
    Never,
}

/// State the Objective-C class needs.
///
/// Flags drained by the run-loop pump rather than callbacks, for the reason
/// given in [`super::pill`]: the pump owns the `ShellRuntime`, and handing a
/// button a `&mut` path into it is a re-entrant borrow waiting to happen.
#[derive(Default)]
struct PromptIvars {
    clicked: Cell<Option<Click>>,
    acknowledgement_changed: Cell<bool>,
}

define_class!(
    /// The panel itself.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "FotwPromptPanel"]
    #[ivars = PromptIvars]
    struct PromptPanel;

    impl PromptPanel {
        /// Borderless windows answer `NO`, which makes the first click on
        /// Start do nothing at all — the worst possible failure for a
        /// consent affordance, because the second click looks like the user
        /// pressing it twice.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        /// Never main: this is an accessory, not the application's window.
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }

        #[unsafe(method(fotwPromptStart:))]
        fn start_clicked(&self, _sender: Option<&AnyObject>) {
            self.ivars().clicked.set(Some(Click::Start));
        }

        #[unsafe(method(fotwPromptNotNow:))]
        fn not_now_clicked(&self, _sender: Option<&AnyObject>) {
            self.ivars().clicked.set(Some(Click::NotNow));
        }

        #[unsafe(method(fotwPromptNever:))]
        fn never_clicked(&self, _sender: Option<&AnyObject>) {
            self.ivars().clicked.set(Some(Click::Never));
        }

        /// The all-party acknowledgement checkbox. Records that it moved; the
        /// pump reads the state and tells the core, which decides what it
        /// means (CON-05).
        #[unsafe(method(fotwPromptAcknowledge:))]
        fn acknowledge_clicked(&self, _sender: Option<&AnyObject>) {
            self.ivars().acknowledgement_changed.set(true);
        }
    }
);

impl PromptPanel {
    fn build(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PromptIvars::default());
        // THE call. The mask goes in here or the non-activating bit is
        // silently dropped and the prompt steals focus from the meeting.
        unsafe {
            msg_send![
                super(this),
                initWithContentRect: frame,
                styleMask: PROMPT_STYLE,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        }
    }
}

/// The prompt panel and everything it draws.
pub(crate) struct Prompt {
    panel: Retained<PromptPanel>,
    content: Retained<NSVisualEffectView>,
    headline: Retained<NSTextField>,
    evidence: Retained<NSTextField>,
    notice: Retained<NSTextField>,
    acknowledge: Retained<NSButton>,
    start: Retained<NSButton>,
    not_now: Retained<NSButton>,
    never: Retained<NSButton>,
    on_screen: bool,
    last: Option<PromptView>,
}

impl Prompt {
    /// Build the panel and its content. Nothing is shown until
    /// [`Prompt::render`] is given a [`PromptView`].
    pub(crate) fn new(mtm: MainThreadMarker) -> Self {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(MIN_WIDTH, 160.0));
        let panel = PromptPanel::build(mtm, frame);

        panel.setLevel(PROMPT_LEVEL);
        panel.setCollectionBehavior(PROMPT_BEHAVIOR);
        panel.setSharingType(PROMPT_SHARING);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        // A question about the meeting the user is looking at must not vanish
        // when they look back at it.
        panel.setHidesOnDeactivate(false);
        // NOT `setFloatingPanel(true)`: it assigns `NSFloatingWindowLevel` (3)
        // over the status level set above, and the prompt then works on a
        // normal desktop and is invisible behind a full-screen call. Read back
        // off the live panel by `examples/shell_probe`.
        //
        // `becomesKeyOnlyIfNeeded` keeps the keyboard where it was: buttons do
        // not need key focus, so clicking Start never takes the user out of
        // whatever they were typing in.
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setMovableByWindowBackground(true);
        // SAFETY: we hold the only strong reference and never send `close`.
        unsafe { panel.setReleasedWhenClosed(false) };

        let content = NSVisualEffectView::new(mtm);
        content.setFrame(frame);
        content.setMaterial(NSVisualEffectMaterial::HUDWindow);
        content.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        content.setState(NSVisualEffectState::Active);
        content.setWantsLayer(true);
        if let Some(layer) = content.layer() {
            layer.setCornerRadius(12.0);
            layer.setMasksToBounds(true);
        }

        let headline = wrapping_label(mtm, 13.0, true);
        let evidence = wrapping_label(mtm, 11.0, false);
        evidence.setTextColor(Some(&NSColor::secondaryLabelColor()));
        let notice = wrapping_label(mtm, 11.0, false);

        // SAFETY: `panel` is a live main-thread object and implements every
        // selector named below.
        let (acknowledge, start, not_now, never) = unsafe {
            (
                NSButton::checkboxWithTitle_target_action(
                    &NSString::from_str(""),
                    Some(&panel),
                    Some(sel!(fotwPromptAcknowledge:)),
                    mtm,
                ),
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(""),
                    Some(&panel),
                    Some(sel!(fotwPromptStart:)),
                    mtm,
                ),
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(""),
                    Some(&panel),
                    Some(sel!(fotwPromptNotNow:)),
                    mtm,
                ),
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(""),
                    Some(&panel),
                    Some(sel!(fotwPromptNever:)),
                    mtm,
                ),
            )
        };
        // Deliberately no key equivalents. A default button would fire on
        // Return, and this panel appears over an application the user is
        // typing into: "start recording everyone" is not something to bind to
        // a keystroke they did not aim at it.

        for view in [&headline, &evidence, &notice] {
            content.addSubview(view);
        }
        content.addSubview(&acknowledge);
        for button in [&start, &not_now, &never] {
            content.addSubview(button);
        }
        panel.setContentView(Some(&content));

        Self {
            panel,
            content,
            headline,
            evidence,
            notice,
            acknowledge,
            start,
            not_now,
            never,
            on_screen: false,
            last: None,
        }
    }

    /// The button pressed since this was last asked, if any.
    ///
    /// The acknowledgement travels with the answer: it is read off the
    /// checkbox the user is actually looking at, so what is sent is what was
    /// on screen.
    pub(crate) fn take_response(&self) -> Option<PromptChoice> {
        let click = self.panel.ivars().clicked.replace(None)?;
        Some(match click {
            Click::Start => PromptChoice::Start {
                acknowledged: self.acknowledgement_is_ticked(),
            },
            Click::NotNow => PromptChoice::NotNow,
            Click::Never => PromptChoice::NeverForThisApp,
        })
    }

    /// The checkbox's state, if it moved since this was last asked.
    pub(crate) fn take_acknowledgement(&self) -> Option<bool> {
        self.panel
            .ivars()
            .acknowledgement_changed
            .replace(false)
            .then(|| self.acknowledgement_is_ticked())
    }

    fn acknowledgement_is_ticked(&self) -> bool {
        self.acknowledge.state() == NSControlStateValueOn
    }

    /// Whether the Start button, as drawn, would accept a click.
    ///
    /// Read off the live `NSButton` rather than from the view that produced
    /// it: the question the probe asks is whether the renderer applied the
    /// core's gate, and only the button can answer that.
    pub(crate) fn start_is_enabled(&self) -> bool {
        self.start.isEnabled()
    }

    /// Whether the panel is actually on screen, inside a screen.
    ///
    /// `orderFrontRegardless` succeeds silently for a window positioned at
    /// (0, 0) with no screen to land on — which is what happens when
    /// `mainScreen` returns `None` and the fallback path is taken. The panel
    /// is then "shown" and invisible.
    pub(crate) fn is_on_a_screen(&self, mtm: MainThreadMarker) -> bool {
        if !self.panel.isVisible() {
            return false;
        }
        let frame = self.panel.frame();
        NSScreen::screens(mtm).iter().any(|screen| {
            let visible = screen.visibleFrame();
            frame.origin.x >= visible.origin.x - 1.0
                && frame.origin.y >= visible.origin.y - 1.0
                && frame.origin.x + frame.size.width <= visible.origin.x + visible.size.width + 1.0
                && frame.origin.y + frame.size.height
                    <= visible.origin.y + visible.size.height + 1.0
        })
    }

    /// Press a control the way a finger would, for [`super::probe`].
    ///
    /// `performClick:` goes through AppKit's real target/action dispatch, so a
    /// mistyped selector raises `unrecognized selector` here rather than under
    /// the user's finger during a meeting — and a **disabled** button
    /// swallows the click, which is how the CON-05 gate is verified as
    /// behaviour rather than as a boolean.
    ///
    /// This sets the same ivar a click sets and nothing else. It cannot start
    /// a recording: the pump is what turns a drained response into a
    /// [`PromptChoice`], and the probe's core is a throwaway wired to no host.
    pub(crate) fn press(&self, control: ProbePress) {
        let button = match control {
            ProbePress::Start => &self.start,
            ProbePress::NotNow => &self.not_now,
            ProbePress::Never => &self.never,
            ProbePress::Acknowledge => &self.acknowledge,
        };
        // SAFETY: a live main-thread control whose target is the panel below.
        unsafe { button.performClick(None) };
    }

    /// What AppKit reports about the panel, read back from the live object.
    ///
    /// Not a copy of the constants that went in: the question is whether
    /// AppKit *kept* them (see [`crate::probe`]).
    pub(crate) fn probe_into(&self, probe: &mut crate::probe::ShellProbe) {
        let mask = self.panel.styleMask();
        probe.prompt_style_mask = mask.0;
        probe.prompt_is_nonactivating = mask.contains(NSWindowStyleMask::NonactivatingPanel);
        probe.prompt_can_become_key = self.panel.canBecomeKeyWindow();
        probe.prompt_can_become_main = self.panel.canBecomeMainWindow();
        probe.prompt_level = self.panel.level();
        probe.prompt_sharing_type = self.panel.sharingType().0;
        probe.prompt_collection_behavior = self.panel.collectionBehavior().0;
    }

    /// Whether every rendered control fits inside the panel it was laid out
    /// in, measured against the live text.
    ///
    /// The failure this catches is specific and silent: a jurisdiction warning
    /// longer than the space reserved for it is *clipped*, so the user reads
    /// half a sentence about criminal liability and presses Start. Height is
    /// derived from the measured text rather than assumed, and this asks
    /// AppKit whether the derivation held.
    pub(crate) fn fits(&self) -> bool {
        let frame = self.panel.frame().size;
        let width = frame.width - PAD * 2.0;
        let mut ok = frame.width > 0.0 && frame.height > 0.0;
        for field in [&self.headline, &self.evidence, &self.notice] {
            let needed = measured_height(field, width);
            ok &= field.frame().size.height + 0.5 >= needed;
            ok &= field.frame().origin.y >= -0.5;
            ok &= field.frame().origin.y + field.frame().size.height <= frame.height + 0.5;
            // A second, independent floor. The check above goes through the
            // same measurement the layout used, so a `measured_height` that
            // returned a constant would agree with itself and report a clipped
            // warning as fitting. Explicit newlines are a lower bound on the
            // line count that needs no font metrics at all -- wrapping only
            // ever adds lines.
            let text = field.stringValue().to_string();
            if !text.is_empty() {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a line count, not a measurement"
                )]
                let floor = text.lines().count() as f64 * MIN_LINE_HEIGHT;
                ok &= field.frame().size.height + 0.5 >= floor;
            }
        }
        // The button row, laid out right to left, must not have run off the
        // left edge -- which is how a long localized label silently eats
        // "Never for this app".
        ok &= self.never.frame().origin.x >= PAD - 0.5;
        ok
    }

    /// Apply a view.
    ///
    /// `None` means nothing is armed, and takes the panel off screen. There is
    /// no other way to reach `orderOut:` from outside this module.
    pub(crate) fn render(&mut self, view: Option<&PromptView>, mtm: MainThreadMarker) {
        let Some(view) = view else {
            if self.on_screen {
                self.panel.orderOut(None);
                self.on_screen = false;
                self.last = None;
            }
            return;
        };

        if self.last.as_ref() != Some(view) {
            self.apply(view, mtm);
            self.last = Some(view.clone());
        }

        if !self.on_screen {
            // Not `makeKeyAndOrderFront:`: showing the prompt must not
            // activate this application over the meeting.
            self.panel.orderFrontRegardless();
            self.on_screen = true;
        }
    }

    /// Set every string, measure what that produced, and lay the panel out
    /// around it.
    fn apply(&self, view: &PromptView, mtm: MainThreadMarker) {
        self.headline
            .setStringValue(&NSString::from_str(&view.headline));
        self.evidence
            .setStringValue(&NSString::from_str(&view.evidence));
        self.notice
            .setStringValue(&NSString::from_str(&view.consent_notice));
        // A blocking warning is drawn in the warning register; a one-party
        // reminder is not, or the register stops meaning anything.
        let notice_color = if view.requires_acknowledgement {
            NSColor::systemOrangeColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        self.notice.setTextColor(Some(&notice_color));

        self.acknowledge
            .setTitle(&NSString::from_str(view.acknowledge_label));
        self.acknowledge.setState(if view.acknowledged {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        self.acknowledge.setHidden(!view.requires_acknowledgement);

        self.start.setTitle(&NSString::from_str(view.start_label));
        self.not_now
            .setTitle(&NSString::from_str(view.not_now_label));
        self.never.setTitle(&NSString::from_str(view.never_label));
        // The core's rule, applied and never recomputed. A greyed Start beside
        // an un-ticked box is the whole of "blocking" as the user sees it;
        // `ShellCore` refuses the same start again underneath.
        self.start.setEnabled(view.start_enabled);

        self.panel
            .setAccessibilityLabel(Some(&NSString::from_str(&view.accessibility_label())));

        for button in [&self.start, &self.not_now, &self.never] {
            button.sizeToFit();
        }
        let buttons_width = [&self.start, &self.not_now, &self.never]
            .iter()
            .map(|b| b.frame().size.width)
            .sum::<f64>()
            + BUTTON_GAP * 2.0;
        let width = panel_width(buttons_width);
        let text_width = width - PAD * 2.0;

        let heights = [
            measured_height(&self.headline, text_width),
            measured_height(&self.evidence, text_width),
            if view.consent_notice.is_empty() {
                0.0
            } else {
                measured_height(&self.notice, text_width)
            },
            if view.requires_acknowledgement {
                CHECKBOX_HEIGHT
            } else {
                0.0
            },
            BUTTON_HEIGHT,
        ];
        let height = panel_height(&heights);

        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
        self.content.setFrame(frame);

        // AppKit's origin is bottom-left; the stack reads top-down, which is
        // the order the panel is read in.
        let mut cursor = height - PAD;
        let mut place = |h: f64| {
            cursor -= h;
            let y = cursor;
            if h > 0.0 {
                cursor -= GAP;
            }
            y
        };
        self.headline.setFrame(NSRect::new(
            NSPoint::new(PAD, place(heights[0])),
            NSSize::new(text_width, heights[0]),
        ));
        self.evidence.setFrame(NSRect::new(
            NSPoint::new(PAD, place(heights[1])),
            NSSize::new(text_width, heights[1]),
        ));
        self.notice.setFrame(NSRect::new(
            NSPoint::new(PAD, place(heights[2])),
            NSSize::new(text_width, heights[2]),
        ));
        self.acknowledge.setFrame(NSRect::new(
            NSPoint::new(PAD, place(heights[3])),
            NSSize::new(text_width, heights[3].max(CHECKBOX_HEIGHT)),
        ));

        let row_y = place(heights[4]);
        let mut right = width - PAD;
        for button in [&self.start, &self.not_now, &self.never] {
            let w = button.frame().size.width;
            right -= w;
            button.setFrame(NSRect::new(
                NSPoint::new(right, row_y),
                NSSize::new(w, BUTTON_HEIGHT),
            ));
            right -= BUTTON_GAP;
        }

        self.reposition(frame.size, mtm);
    }

    /// Top-right of the active screen, under the menu bar.
    ///
    /// Deliberately not centred over the meeting window: this is a question
    /// from the menu-bar item, and it should not land on top of the face of
    /// whoever is talking.
    fn reposition(&self, size: NSSize, mtm: MainThreadMarker) {
        let Some(screen) = NSScreen::mainScreen(mtm) else {
            self.panel
                .setFrame_display(NSRect::new(NSPoint::new(0.0, 0.0), size), true);
            return;
        };
        let visible = screen.visibleFrame();
        let x = visible.origin.x + visible.size.width - size.width - SCREEN_MARGIN;
        let y = visible.origin.y + visible.size.height - size.height - SCREEN_MARGIN;
        self.panel
            .setFrame_display(NSRect::new(NSPoint::new(x, y), size), true);
    }
}

/// How wide the panel has to be to hold its button row, clamped.
fn panel_width(buttons_width: f64) -> f64 {
    (buttons_width + PAD * 2.0).clamp(MIN_WIDTH, MAX_WIDTH)
}

/// The panel's height for a stack of row heights. Zero-height rows take no gap.
fn panel_height(rows: &[f64]) -> f64 {
    let drawn = rows.iter().filter(|h| **h > 0.0).count();
    let gaps = f64::from(u32::try_from(drawn.saturating_sub(1)).unwrap_or(0));
    PAD * 2.0 + rows.iter().sum::<f64>() + GAP * gaps
}

/// A non-selectable label that wraps.
///
/// `labelWithString:` rather than `wrappingLabelWithString:` — see the module
/// docs: a selectable field would take the keyboard away from the meeting.
/// `setWraps(true)` on the cell turns on word wrapping without making the
/// field selectable, and without pulling in `NSParagraphStyle`.
fn wrapping_label(mtm: MainThreadMarker, size: f64, bold: bool) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    field.setFont(Some(&font));
    field.setMaximumNumberOfLines(0);
    if let Some(cell) = field.cell() {
        cell.setWraps(true);
        // Without this, a line too long to fit is silently replaced by an
        // ellipsis -- which, on the jurisdiction warning, means the user reads
        // "Recording without every participant's consent is a…".
        cell.setTruncatesLastVisibleLine(false);
    }
    field
}

/// How tall `field` needs to be to show all of its text at `width`.
///
/// Asked of AppKit rather than estimated from a character count: the font is
/// the system font at the user's own text size, and a guess that is wrong in
/// the short direction clips a legal warning.
fn measured_height(field: &NSTextField, width: f64) -> f64 {
    let Some(cell) = field.cell() else {
        return 18.0;
    };
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, 10_000.0));
    cell.cellSizeForBounds(bounds).height.ceil()
}

/// Guards on the constants and the arithmetic, not on behaviour.
///
/// **These prove nothing about what is on screen.** No CI runner has a window
/// server. What they catch is the specific regression each trap describes, and
/// a layout that stops growing with its text. `crates/fotw-shell/QA.md` §6b and
/// `examples/shell_probe` are what cover the rest.
#[cfg(test)]
mod tests {
    use super::{
        BUTTON_HEIGHT, GAP, MAX_WIDTH, MIN_WIDTH, PAD, PROMPT_BEHAVIOR, PROMPT_LEVEL,
        PROMPT_SHARING, PROMPT_STYLE, panel_height, panel_width,
    };
    use objc2_app_kit::{
        NSFloatingWindowLevel, NSStatusWindowLevel, NSWindowCollectionBehavior,
        NSWindowSharingType, NSWindowStyleMask,
    };

    #[test]
    fn the_style_mask_carries_the_non_activating_bit() {
        assert!(
            PROMPT_STYLE.contains(NSWindowStyleMask::NonactivatingPanel),
            "without this bit -- passed to the initializer, not to setStyleMask: -- \
             the prompt takes focus off the meeting the moment it appears"
        );
    }

    #[test]
    fn the_prompt_sits_at_status_level_not_floating_level() {
        assert_eq!(PROMPT_LEVEL, NSStatusWindowLevel);
        assert_ne!(
            PROMPT_LEVEL, NSFloatingWindowLevel,
            "the prompt fires while the user is in a call, which is exactly when \
             that call is full-screen: at level 3 nobody ever sees it"
        );
    }

    #[test]
    fn the_prompt_is_excluded_from_screen_capture() {
        assert_eq!(PROMPT_SHARING, NSWindowSharingType::None);
    }

    #[test]
    fn the_collection_behaviour_survives_a_full_screen_space() {
        for bit in [
            NSWindowCollectionBehavior::CanJoinAllSpaces,
            NSWindowCollectionBehavior::FullScreenAuxiliary,
            NSWindowCollectionBehavior::Stationary,
            NSWindowCollectionBehavior::IgnoresCycle,
        ] {
            assert!(PROMPT_BEHAVIOR.contains(bit));
        }
    }

    #[test]
    fn the_panel_grows_with_its_consent_notice() {
        // The clipping regression, in arithmetic: a taller warning must
        // produce a taller panel, monotonically, with no ceiling.
        let short = panel_height(&[20.0, 16.0, 16.0, 0.0, BUTTON_HEIGHT]);
        let long = panel_height(&[20.0, 16.0, 160.0, 0.0, BUTTON_HEIGHT]);
        assert!(long > short);
        assert!((long - short - 144.0).abs() < f64::EPSILON);

        let mut previous = 0.0;
        for lines in 0..40 {
            let h = panel_height(&[20.0, 16.0, f64::from(lines) * 14.0, 0.0, BUTTON_HEIGHT]);
            assert!(h > previous, "height stopped growing at {lines} lines");
            previous = h;
        }
    }

    #[test]
    fn a_row_that_is_not_drawn_takes_no_space_at_all() {
        // The one-party case: no checkbox, and no gap where it would have been.
        let with_box = panel_height(&[20.0, 16.0, 16.0, 20.0, BUTTON_HEIGHT]);
        let without = panel_height(&[20.0, 16.0, 16.0, 0.0, BUTTON_HEIGHT]);
        assert!((with_box - without - (20.0 + GAP)).abs() < f64::EPSILON);
    }

    #[test]
    fn the_panel_always_leaves_room_for_its_padding() {
        assert!((panel_height(&[]) - PAD * 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_panel_widens_for_a_button_row_that_would_not_fit() {
        // Points, compared to the nearest hundredth: exact float equality is
        // both unnecessary here and a clippy error.
        let close = |a: f64, b: f64| (a - b).abs() < 0.01;
        assert!(close(panel_width(10.0), MIN_WIDTH), "never narrower");
        assert!(
            close(panel_width(MIN_WIDTH), MIN_WIDTH + PAD * 2.0),
            "a button row wider than the floor must widen the panel, not be clipped"
        );
        assert!(
            close(panel_width(10_000.0), MAX_WIDTH),
            "and never unbounded"
        );
    }
}
