//! The AppKit shell.
//!
//! **This is the only directory in the crate permitted to name AppKit
//! types**, mirroring the rule `fotw-audio` applies to Core Audio
//! (docs/REQUIREMENTS.md 6.5). Everything above it sees
//! [`ShellRuntime`](crate::ShellRuntime) and plain data.
//!
//! # Structure
//!
//! `NSApplication::run()` never returns and must own the main thread, so this
//! layer is a renderer and nothing else. It pumps the OS event sources into
//! the runtime, then paints [`ShellRuntime::view`](crate::ShellRuntime::view).
//! Every decision lives in [`ShellCore`](crate::ShellCore), which is where the
//! tests are.
//!
//! # Activation policy
//!
//! `LSUIElement` in `packaging/Info.plist` **and**
//! `setActivationPolicy(Accessory)` here. Either one alone is wrong:
//! `LSUIElement` covers the LaunchServices path, the runtime call covers
//! every other way the binary gets started (a `launchd` job, a direct
//! `exec`, `cargo run` against a loose binary with no bundle at all), and
//! without it those paths get a Dock icon and a menu bar.
//!
//! # What is not proven here
//!
//! No test in CI reaches this module: GitHub's macOS runners have no window
//! server. The properties that matter (does the pill stay above a full-screen
//! Zoom, does clicking Stop steal focus, is it excluded from a screen share)
//! are observable only on a real desktop with a real meeting on it. Those are
//! `crates/fotw-shell/QA.md` line items, per release. Claiming otherwise would
//! be worse than claiming nothing.
//!
//! [`probe`] is the one thing in between. It runs on a developer's machine
//! rather than in CI, brings the surfaces up, and asks **AppKit** what it made
//! of them — including pressing the prompt's buttons through real
//! target/action dispatch. It cannot see a layout that is ugly or a panel
//! behind a full-screen space; it can see a selector that goes nowhere and a
//! consent gate the renderer forgot to apply.

mod hotkeys;
mod pill;
mod prompt;
mod tray;

use std::cell::RefCell;
use std::convert::Infallible;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Instant;

use block2::RcBlock;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use muda::MenuEvent;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSTimeInterval, NSTimer};
use tray_icon::TrayIconEvent;

use crate::clock::Monotonic;
use crate::error::ShellError;
use crate::hotkey::HotkeyMap;
use crate::platform::macos::hotkeys::HotkeyRegistrar;
use crate::platform::macos::pill::Pill;
use crate::platform::macos::prompt::{ProbePress, Prompt};
use crate::platform::macos::tray::Tray;
use crate::probe::ShellProbe;
use crate::prompt::{DetectedMeeting, PromptChoice};
use crate::runtime::{ShellHost, ShellRuntime};
use crate::state::{ShellCore, ShellInput};
use crate::view::MenuAction;

/// How often the run loop drains the event channels and advances the clock.
///
/// `tray-icon`, `muda` and `global-hotkey` all deliver through global
/// `crossbeam` channels rather than callbacks we can hook, so something has to
/// poll them. 50 ms is under the threshold where a menu click feels delayed
/// and is three orders of magnitude cheaper than the 3% CPU budget in
/// docs/REQUIREMENTS.md 5.5. It also bounds how long those unbounded channels
/// can accumulate events.
const PUMP_INTERVAL: NSTimeInterval = 0.05;

/// Run the shell. Does not return.
///
/// Installs the accessory activation policy, creates the menu-bar item, the
/// recording pill and the global hotkeys, then hands the main thread to
/// AppKit.
///
/// # Errors
///
/// If called off the main thread, or if any of the three surfaces cannot be
/// created. A shell that came up without its menu-bar item would be a shell
/// with no recording indicator, so a partial start is a failure, not a
/// degraded mode (CON-02).
pub fn run<H: ShellHost + 'static>(host: H, hotkeys: HotkeyMap) -> Result<Infallible, ShellError> {
    let mtm = MainThreadMarker::new().ok_or(ShellError::NotMainThread)?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let shell = Rc::new(RefCell::new(MacShell::new(mtm, host, hotkeys)?));
    shell.borrow_mut().render();

    // Weak, so the timer cannot keep the shell alive past teardown, and
    // `try_borrow_mut` so a re-entrant fire during a render is dropped rather
    // than panicking inside AppKit.
    let weak = Rc::downgrade(&shell);
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        if let Some(rc) = weak.upgrade()
            && let Ok(mut shell) = rc.try_borrow_mut()
        {
            shell.pump();
        }
    });
    // SAFETY: the block only touches main-thread state and is only ever
    // invoked by the main run loop, so it is trivially "sendable" in the sense
    // the binding asks about.
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(PUMP_INTERVAL, true, &block)
    };

    // Both live for the life of the process. Dropping either stops the pump,
    // which would freeze the elapsed clock on the recording indicator while
    // capture continued.
    std::mem::forget(timer);
    std::mem::forget(shell);

    app.run();

    // `run` returns only after `[NSApp stop:]`, at which point there is
    // nothing left to do and no value of `Infallible` to produce.
    std::process::exit(0);
}

/// Build the three surfaces, ask AppKit what it made of them, tear them down.
///
/// This is the self-check behind `fotw doctor`, and the only way to observe
/// whether the non-activating style mask survived `NSPanel` initialization —
/// see [`crate::probe`]. It creates a real status item, so the menu-bar icon
/// flashes for as long as the call takes.
///
/// # Errors
///
/// If called off the main thread, or if any surface fails to come up.
///
/// # Panics
///
/// If arming the state machine from idle produces no prompt, which would mean
/// `ShellCore` had stopped honouring a detection at all — a defect the whole
/// consent flow rests on, and one a diagnostic tool should fail loudly on
/// rather than report as a healthy shell.
pub fn probe() -> Result<ShellProbe, ShellError> {
    let mtm = MainThreadMarker::new().ok_or(ShellError::NotMainThread)?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let hotkeys = HotkeyMap::defaults();
    let registrar = HotkeyRegistrar::register(&hotkeys)?;
    let tray = Tray::new(&ShellCore::new().view().menu)?;
    let pill = Pill::new(mtm);
    let mut prompt = Prompt::new(mtm);

    // Every field starts at the value that fails, so a check that is somehow
    // skipped reports unhealthy rather than passing by default.
    let mut probe = ShellProbe {
        activation_policy: app.activationPolicy().0,
        panel_style_mask: 0,
        panel_is_nonactivating: false,
        panel_can_become_key: false,
        panel_can_become_main: true,
        panel_level: 0,
        panel_sharing_type: usize::MAX,
        panel_collection_behavior: 0,
        prompt_style_mask: 0,
        prompt_is_nonactivating: false,
        prompt_can_become_key: false,
        prompt_can_become_main: true,
        prompt_level: 0,
        prompt_sharing_type: usize::MAX,
        prompt_collection_behavior: 0,
        prompt_blocking_start_disabled: false,
        prompt_acknowledged_start_enabled: false,
        prompt_content_fits: false,
        prompt_is_on_a_screen: false,
        prompt_disabled_start_swallows_the_click: false,
        prompt_checkbox_dispatches: false,
        prompt_start_click_dispatches: false,
        prompt_dismissals_dispatch: false,
        status_item_retained: true,
        hotkeys_registered: hotkeys.len(),
    };
    pill.probe_into(&mut probe);

    // Drive the real state machine through the blocking-consent case and read
    // the answers off the live controls. This is the part no unit test can
    // reach: whether the panel *applied* the core's gate, whether the warning
    // it is gating on is legible rather than clipped, and whether a click on
    // any of it reaches anything at all.
    let mut core = ShellCore::new();
    core.handle(ShellInput::MeetingDetected {
        at: Monotonic::ZERO,
        meeting: probe_meeting(),
    });
    let view = core.view().prompt.expect("a detection arms a prompt");
    prompt.render(Some(&view), mtm);
    probe.prompt_blocking_start_disabled = !prompt.start_is_enabled();
    probe.prompt_content_fits = prompt.fits();
    probe.prompt_is_on_a_screen = prompt.is_on_a_screen(mtm);

    // CON-05 as behaviour: a real click on the disabled Start must produce
    // nothing. A `start_enabled: false` that the renderer forgot to apply
    // looks identical to this in every test we can run in CI.
    prompt.press(ProbePress::Start);
    probe.prompt_disabled_start_swallows_the_click = prompt.take_response().is_none();

    // The checkbox, through AppKit's own target/action dispatch.
    prompt.press(ProbePress::Acknowledge);
    let ticked = prompt.take_acknowledgement();
    probe.prompt_checkbox_dispatches = ticked == Some(true);
    core.handle(ShellInput::PromptAcknowledged {
        acknowledged: ticked.unwrap_or(false),
    });

    let view = core.view().prompt.expect("the prompt is still up");
    prompt.render(Some(&view), mtm);
    probe.prompt_acknowledged_start_enabled = prompt.start_is_enabled();
    probe.prompt_content_fits &= prompt.fits();

    prompt.press(ProbePress::Start);
    probe.prompt_start_click_dispatches =
        prompt.take_response() == Some(PromptChoice::Start { acknowledged: true });

    prompt.press(ProbePress::NotNow);
    let not_now = prompt.take_response();
    prompt.press(ProbePress::Never);
    let never = prompt.take_response();
    probe.prompt_dismissals_dispatch =
        not_now == Some(PromptChoice::NotNow) && never == Some(PromptChoice::NeverForThisApp);

    prompt.probe_into(&mut probe);
    prompt.render(None, mtm);

    drop(tray);
    drop(registrar);
    Ok(probe)
}

/// The prompt the probe renders: an all-party jurisdiction with a long,
/// wrapping citation.
///
/// A fixture rather than a call into `fotwd`'s consent engine, which this
/// crate cannot depend on — but it mirrors the shipped default, because
/// `DetectorConfig::home_jurisdiction` is `US-CA` and California is all-party.
/// **The blocking case is what a fresh install draws**, not an edge case, so
/// it is the one the probe measures.
fn probe_meeting() -> DetectedMeeting {
    DetectedMeeting::new("us.zoom.xos", "Zoom", "Zoom is using the microphone")
        .with_title("Weekly design review with the platform team")
        .with_consent_notice(
            "These jurisdictions require every participant's consent:\n  \
             • California — Cal. Penal Code § 632 (https://leginfo.legislature.ca.gov/faces/\
             codes_displaySection.xhtml?lawCode=PEN&sectionNum=632)\n\
             This is not legal advice.",
            true,
        )
}

struct MacShell<H: ShellHost> {
    runtime: ShellRuntime<H>,
    tray: Tray,
    pill: Pill,
    prompt: Prompt,
    hotkeys: HotkeyRegistrar,
    origin: Instant,
    mtm: MainThreadMarker,
}

impl<H: ShellHost> MacShell<H> {
    fn new(mtm: MainThreadMarker, host: H, hotkeys: HotkeyMap) -> Result<Self, ShellError> {
        let registrar = HotkeyRegistrar::register(&hotkeys)?;
        let runtime = ShellRuntime::with_hotkeys(host, hotkeys);
        let tray = Tray::new(&runtime.view().menu)?;
        let pill = Pill::new(mtm);
        let prompt = Prompt::new(mtm);
        Ok(Self {
            runtime,
            tray,
            pill,
            prompt,
            hotkeys: registrar,
            origin: Instant::now(),
            mtm,
        })
    }

    fn now(&self) -> Monotonic {
        Monotonic::from_duration(self.origin.elapsed())
    }

    fn pump(&mut self) {
        let now = self.now();

        // Global hotkeys. `Pressed` only: acting on both edges would toggle
        // recording twice per keypress.
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.state != HotKeyState::Pressed {
                continue;
            }
            if let Some(chord) = self.hotkeys.chord_for(event.id) {
                self.runtime.on_chord(chord, now);
            }
        }

        // Menu clicks.
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(action) = MenuAction::from_id(&event.id.0) {
                self.runtime.on_menu(action, now);
            }
        }

        // Tray clicks. We take no action on them -- the menu opens by itself
        // -- but the channel is unbounded and would otherwise grow for the
        // life of the process.
        while TrayIconEvent::receiver().try_recv().is_ok() {}

        // The pill's Stop button.
        if self.pill.take_stop_request() {
            self.runtime.request_stop();
        }

        // The prompt. The acknowledgement is applied first: if a user manages
        // to tick the box and press Start inside one 50 ms poll, the tick was
        // still first in wall-clock order and has to be honoured that way
        // (CON-05).
        if let Some(acknowledged) = self.prompt.take_acknowledgement() {
            self.runtime.acknowledge_prompt(acknowledged);
        }
        if let Some(choice) = self.prompt.take_response() {
            self.runtime.respond_to_prompt(choice, now);
        }

        self.runtime.tick(now);
        self.render();
    }

    fn render(&mut self) {
        let view = self.runtime.view();
        self.tray.render(&view.tray, &view.menu);
        self.pill.render(view.pill.as_ref(), self.mtm);
        // Issue #52: this line is the feature. Without it the state machine
        // arms, the audit log is correct, and no human is ever asked.
        self.prompt.render(view.prompt.as_ref(), self.mtm);
    }
}
