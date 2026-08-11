//! The recording pill: an `NSPanel` subclass, built by hand.
//!
//! # The one detail that decides this whole file
//!
//! `NSWindowStyleMask::NonactivatingPanel` (1<<7) **is only honoured if it is
//! passed to the initializer.** AppKit calls the private
//! `-_setPreventsActivation:` — which flips the WindowServer's
//! `kCGSPreventsActivationTagBit` — during panel initialization, and never
//! from `setStyleMask:` (FB16484811).
//!
//! That is why `tauri-nspanel` and `tao` cannot do this: they create an
//! `NSWindow`, `object_setClass` it to a panel subclass, then call
//! `setStyleMask:`, producing a window that looks key but receives no key
//! events. We call
//! [`NSPanel::initWithContentRect_styleMask_backing_defer`] with the mask
//! already in it (docs/REQUIREMENTS.md 5.5).
//!
//! Getting this wrong means clicking Stop on the pill steals focus from Zoom
//! mid-call — at the exact moment it must not.
//!
//! # The other three traps, all of which fail silently
//!
//! - **`NSStatusWindowLevel` (25), not `NSFloatingWindowLevel` (3).** Floating
//!   is not high enough to sit above another application's full-screen space
//!   even with `FullScreenAuxiliary`. The pill works perfectly on a normal
//!   desktop and vanishes the moment the user full-screens Zoom — which is
//!   precisely the scenario it exists for, and one you never hit in local dev.
//! - **A borderless window returns `NO` from `canBecomeKeyWindow`**, so its
//!   buttons swallow the first click. Overridden below.
//! - **`setSharingType(NSWindowSharingNone)`** keeps the pill out of the
//!   user's own screen share. Deprecated in favour of ScreenCaptureKit
//!   content filters, so it is a QA.md line item, not a claim we make in copy
//!   (docs/REQUIREMENTS.md 5.5).
//!
//! # CON-02
//!
//! Nothing in this module's API can take the pill down. [`Pill::render`] is
//! the only public mutator and it hides the panel exactly when
//! [`ShellCore::view`](crate::ShellCore::view) says there is nothing to
//! indicate. There is no `hide`, no `set_visible`, and no way to reach
//! `orderOut:` from outside.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSBackingStoreType, NSButton, NSColor, NSFont, NSPanel, NSScreen,
    NSStatusWindowLevel, NSTextField, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior, NSWindowLevel,
    NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::state::METER_SEGMENTS;
use crate::view::{PillView, Tone};

/// Pill size, in points.
const PILL_SIZE: NSSize = NSSize {
    width: 268.0,
    height: 36.0,
};

/// Gap between the pill and the bottom of the screen's visible frame.
const BOTTOM_MARGIN: f64 = 24.0;

/// **The style mask, assembled here so it can only ever be passed to the
/// initializer.**
///
/// `Borderless` is zero and contributes nothing but intent;
/// `FullSizeContentView` keeps the content view spanning the whole frame now
/// that there is no title bar.
const PILL_STYLE: NSWindowStyleMask = NSWindowStyleMask(
    NSWindowStyleMask::NonactivatingPanel.0
        | NSWindowStyleMask::Borderless.0
        | NSWindowStyleMask::FullSizeContentView.0,
);

/// Sits above another application's full-screen space, on every space, and
/// out of the ⌘-tab cycle.
const PILL_BEHAVIOR: NSWindowCollectionBehavior = NSWindowCollectionBehavior(
    NSWindowCollectionBehavior::CanJoinAllSpaces.0
        | NSWindowCollectionBehavior::FullScreenAuxiliary.0
        | NSWindowCollectionBehavior::Stationary.0
        | NSWindowCollectionBehavior::IgnoresCycle.0,
);

/// `NSStatusWindowLevel` (25).
///
/// Named rather than inlined so the value is something a test can pin. The
/// regression this guards against is not a typo, it is a plausible tidy-up:
/// `NSFloatingWindowLevel` (3) is the level every "always on top" tutorial
/// uses, it works perfectly on a normal desktop, and it fails only once
/// somebody full-screens their meeting window.
const PILL_LEVEL: NSWindowLevel = NSStatusWindowLevel;

/// Keeps the pill out of the user's own screen share.
///
/// Deprecated by Apple in favour of ScreenCaptureKit content filters, so
/// **this needs a behaviour test on a real desktop before any copy claims the
/// overlay is invisible during screen share** (docs/REQUIREMENTS.md 5.5).
const PILL_SHARING: NSWindowSharingType = NSWindowSharingType::None;

/// State the Objective-C class needs.
#[derive(Default)]
struct PillIvars {
    /// Set by the Stop button, drained by the pump.
    ///
    /// A flag rather than a callback: the run-loop pump already owns the
    /// `ShellRuntime`, and handing an Objective-C class a `&mut` path into it
    /// would mean a re-entrant borrow the first time a click landed inside a
    /// render.
    stop_requested: Cell<bool>,
}

define_class!(
    /// The panel itself.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "FotwRecordingPanel"]
    #[ivars = PillIvars]
    struct RecordingPanel;

    impl RecordingPanel {
        /// A borderless window answers `NO` by default, which makes its
        /// buttons eat the first click. With the non-activating mask set at
        /// init, answering `YES` here gets key events **without** the panel
        /// stealing activation from the meeting app.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        /// Never main. A main window would put this panel in charge of the
        /// application's menu bar and window ordering, which is the opposite
        /// of an accessory indicator.
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }

        #[unsafe(method(fotwStopClicked:))]
        fn stop_clicked(&self, _sender: Option<&AnyObject>) {
            self.ivars().stop_requested.set(true);
        }
    }
);

impl RecordingPanel {
    fn build(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PillIvars::default());
        // THE call. The mask goes in here or it does not take effect at all.
        unsafe {
            msg_send![
                super(this),
                initWithContentRect: frame,
                styleMask: PILL_STYLE,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        }
    }
}

/// The pill, and everything it draws.
pub(crate) struct Pill {
    panel: Retained<RecordingPanel>,
    status: Retained<NSTextField>,
    elapsed: Retained<NSTextField>,
    meter: Retained<NSTextField>,
    stop: Retained<NSButton>,
    on_screen: bool,
    last: Option<PillView>,
}

impl Pill {
    /// Build the panel and its content. Nothing is shown until
    /// [`Pill::render`] is given a [`PillView`].
    pub(crate) fn new(mtm: MainThreadMarker) -> Self {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), PILL_SIZE);
        let panel = RecordingPanel::build(mtm, frame);

        panel.setLevel(PILL_LEVEL);
        panel.setCollectionBehavior(PILL_BEHAVIOR);
        panel.setSharingType(PILL_SHARING);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        // An accessory indicator that disappears when the user clicks back
        // into Zoom would indicate nothing at all.
        panel.setHidesOnDeactivate(false);
        // NOT `setFloatingPanel(true)`. It reads like a harmless "keep this on
        // top" hint, but `-[NSPanel setFloatingPanel:]` *assigns the window
        // level*: YES sets `NSFloatingWindowLevel` (3), silently overwriting
        // the `NSStatusWindowLevel` (25) set above. The pill then works
        // perfectly on a normal desktop and disappears the moment the user
        // full-screens their meeting window -- the exact failure
        // docs/REQUIREMENTS.md 5.5 warns about, arrived at from the other
        // direction. Caught by `examples/shell_probe`, which reads the level
        // back off the live panel; no constant assertion can see it.
        panel.setBecomesKeyOnlyIfNeeded(true);
        // Draggable out of the way, but there is nowhere to drag it to that
        // takes it off screen.
        panel.setMovableByWindowBackground(true);
        // SAFETY: we hold the only strong reference and never send `close`.
        // Without this, an accidental close would over-release the panel.
        unsafe { panel.setReleasedWhenClosed(false) };

        let content = NSVisualEffectView::new(mtm);
        content.setFrame(frame);
        content.setMaterial(NSVisualEffectMaterial::HUDWindow);
        content.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        content.setState(NSVisualEffectState::Active);
        content.setWantsLayer(true);
        if let Some(layer) = content.layer() {
            layer.setCornerRadius(PILL_SIZE.height / 2.0);
            layer.setMasksToBounds(true);
        }

        let status = label(mtm, "Recording", 12.0);
        status.setFrame(NSRect::new(
            NSPoint::new(16.0, 9.0),
            NSSize::new(82.0, 18.0),
        ));

        let elapsed = label(mtm, "00:00", 15.0);
        elapsed.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
            15.0, 0.3,
        )));
        elapsed.setFrame(NSRect::new(
            NSPoint::new(100.0, 8.0),
            NSSize::new(56.0, 20.0),
        ));

        let meter = label(mtm, "", 11.0);
        meter.setTextColor(Some(&NSColor::secondaryLabelColor()));
        meter.setFrame(NSRect::new(
            NSPoint::new(158.0, 10.0),
            NSSize::new(52.0, 16.0),
        ));

        let stop_title = NSString::from_str("Stop");
        // SAFETY: `panel` is a live object on the main thread and responds to
        // `fotwStopClicked:`, which is defined on it above.
        let stop = unsafe {
            NSButton::buttonWithTitle_target_action(
                &stop_title,
                Some(&panel),
                Some(sel!(fotwStopClicked:)),
                mtm,
            )
        };
        stop.setFrame(NSRect::new(
            NSPoint::new(212.0, 5.0),
            NSSize::new(48.0, 26.0),
        ));

        content.addSubview(&status);
        content.addSubview(&elapsed);
        content.addSubview(&meter);
        content.addSubview(&stop);
        panel.setContentView(Some(&content));

        Self {
            panel,
            status,
            elapsed,
            meter,
            stop,
            on_screen: false,
            last: None,
        }
    }

    /// Whether the Stop button was clicked since this was last asked.
    pub(crate) fn take_stop_request(&self) -> bool {
        self.panel.ivars().stop_requested.replace(false)
    }

    /// What AppKit reports about the panel, read back from the live object.
    ///
    /// Deliberately not a copy of the constants that went in: the whole
    /// question is whether AppKit *kept* the non-activating bit, and only a
    /// running window server can answer that.
    pub(crate) fn probe_into(&self, probe: &mut crate::probe::ShellProbe) {
        let mask = self.panel.styleMask();
        probe.panel_style_mask = mask.0;
        probe.panel_is_nonactivating = mask.contains(NSWindowStyleMask::NonactivatingPanel);
        probe.panel_can_become_key = self.panel.canBecomeKeyWindow();
        probe.panel_can_become_main = self.panel.canBecomeMainWindow();
        probe.panel_level = self.panel.level();
        probe.panel_sharing_type = self.panel.sharingType().0;
        probe.panel_collection_behavior = self.panel.collectionBehavior().0;
    }

    /// Apply a view.
    ///
    /// `None` means the core says there is nothing to indicate, and is the
    /// **only** path that takes the panel off screen (CON-02).
    pub(crate) fn render(&mut self, view: Option<&PillView>, mtm: MainThreadMarker) {
        let Some(view) = view else {
            if self.on_screen {
                self.panel.orderOut(None);
                self.on_screen = false;
                self.last = None;
            }
            return;
        };

        if self.last.as_ref() != Some(view) {
            self.apply(view);
            self.last = Some(view.clone());
        }

        if !self.on_screen {
            self.reposition(mtm);
            // Not `makeKeyAndOrderFront:`. `orderFrontRegardless` shows the
            // panel without activating this application, which is the whole
            // reason for the non-activating mask.
            self.panel.orderFrontRegardless();
            self.on_screen = true;
        }
    }

    fn apply(&self, view: &PillView) {
        self.status
            .setStringValue(&NSString::from_str(view.status_label));
        self.status.setTextColor(Some(&tone_color(view.tone)));
        self.elapsed
            .setStringValue(&NSString::from_str(&view.elapsed_label));
        self.meter
            .setStringValue(&NSString::from_str(&view.level.meter(METER_SEGMENTS)));
        self.stop.setEnabled(view.stop_enabled);
        self.panel
            .setAccessibilityLabel(Some(&NSString::from_str(&view.accessibility_label())));
    }

    fn reposition(&self, mtm: MainThreadMarker) {
        let Some(screen) = NSScreen::mainScreen(mtm) else {
            return;
        };
        let visible = screen.visibleFrame();
        let x = visible.origin.x + (visible.size.width - PILL_SIZE.width) / 2.0;
        let y = visible.origin.y + BOTTOM_MARGIN;
        self.panel
            .setFrame_display(NSRect::new(NSPoint::new(x, y), PILL_SIZE), true);
    }
}

fn label(mtm: MainThreadMarker, text: &str, size: f64) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFont(Some(&NSFont::systemFontOfSize(size)));
    field
}

fn tone_color(tone: Tone) -> Retained<NSColor> {
    match tone {
        Tone::Live => NSColor::systemRedColor(),
        Tone::Finishing => NSColor::systemYellowColor(),
        Tone::Saved => NSColor::secondaryLabelColor(),
        Tone::Fault => NSColor::systemOrangeColor(),
    }
}

/// Guards on the constants, not on behaviour.
///
/// **These prove nothing about what happens on screen.** A test cannot open an
/// `NSPanel` on a runner with no window server, and the properties that
/// actually matter — the panel stays above a full-screen Zoom, clicking Stop
/// does not steal focus, the pill is absent from a screen recording — are only
/// observable on a real desktop. Those are `crates/fotw-shell/QA.md` items.
///
/// What these *do* catch is the specific regression each trap describes: a
/// later edit dropping the non-activating bit, or "simplifying"
/// `NSStatusWindowLevel` to `NSFloatingWindowLevel` because both look like
/// "on top" and the difference only shows up in a full-screen space.
#[cfg(test)]
mod tests {
    use super::{PILL_BEHAVIOR, PILL_LEVEL, PILL_SHARING, PILL_STYLE};
    use objc2_app_kit::{
        NSFloatingWindowLevel, NSStatusWindowLevel, NSWindowCollectionBehavior,
        NSWindowSharingType, NSWindowStyleMask,
    };

    #[test]
    fn the_style_mask_carries_the_non_activating_bit() {
        assert!(
            PILL_STYLE.contains(NSWindowStyleMask::NonactivatingPanel),
            "without this bit -- passed to the initializer, not to setStyleMask: -- \
             clicking the pill steals focus from the meeting app"
        );
        assert!(PILL_STYLE.contains(NSWindowStyleMask::FullSizeContentView));
    }

    #[test]
    fn the_pill_sits_at_status_level_not_floating_level() {
        assert_eq!(NSStatusWindowLevel, 25);
        assert_eq!(NSFloatingWindowLevel, 3);
        assert_eq!(
            PILL_LEVEL, NSStatusWindowLevel,
            "floating (3) is not high enough to sit above another app's \
             full-screen space, even with FullScreenAuxiliary -- the pill would \
             work on a normal desktop and vanish the moment Zoom went full screen"
        );
        assert_ne!(PILL_LEVEL, NSFloatingWindowLevel);
    }

    #[test]
    fn the_pill_is_excluded_from_screen_capture() {
        assert_eq!(
            PILL_SHARING,
            NSWindowSharingType::None,
            "the indicator must not leak into the user's own screen share"
        );
    }

    #[test]
    fn the_collection_behaviour_survives_a_full_screen_space() {
        for (bit, why) in [
            (
                NSWindowCollectionBehavior::CanJoinAllSpaces,
                "the pill must follow the user between spaces",
            ),
            (
                NSWindowCollectionBehavior::FullScreenAuxiliary,
                "without this the pill is hidden by a full-screen meeting window",
            ),
            (
                NSWindowCollectionBehavior::Stationary,
                "Mission Control must not animate the indicator away",
            ),
            (
                NSWindowCollectionBehavior::IgnoresCycle,
                "an indicator has no business in the cmd-tab cycle",
            ),
        ] {
            assert!(PILL_BEHAVIOR.contains(bit), "{why}");
        }
    }
}
