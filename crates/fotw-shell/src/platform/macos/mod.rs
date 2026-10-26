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

mod app_delegate;
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
use objc2_app_kit::{NSAlert, NSApplication, NSApplicationActivationPolicy};
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
use crate::surface::{FrameAdmission, SurfaceEvent, SurfaceHost, render_surface};
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
    let runtime = ShellRuntime::with_hotkeys(host, hotkeys);
    let keys = runtime.hotkeys().clone();
    run_surface(runtime, &keys)
}

/// Run a recorder-driven surface host on the AppKit main thread.
///
/// # Errors
/// If called off the main thread or native controls cannot be created.
pub fn run_surface<H: SurfaceHost + 'static>(
    host: H,
    hotkeys: &HotkeyMap,
) -> Result<Infallible, ShellError> {
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

/// Initialize AppKit immediately, then create controls only after background
/// daemon ownership is confirmed. Returns normally after coordinated Quit so
/// the caller can cancel background work and bound runtime shutdown.
///
/// # Errors
/// Startup failure, an off-main-thread call, or failure to create native controls.
pub fn run_bootstrapped<H: SurfaceHost + 'static>(
    receiver: std::sync::mpsc::Receiver<Result<Option<H>, String>>,
    hotkeys: &HotkeyMap,
) -> Result<(), String> {
    use crate::bootstrap::{BootstrapUpdate, SurfaceBootstrap};
    use objc2::runtime::ProtocolObject;
    let delegate = app_delegate::AppDelegate::new(
        MainThreadMarker::new().ok_or_else(|| ShellError::NotMainThread.to_string())?,
    );
    let mtm = MainThreadMarker::new().ok_or_else(|| ShellError::NotMainThread.to_string())?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    let shell: Rc<RefCell<Option<MacShell<H>>>> = Rc::new(RefCell::new(None));
    let result = Rc::new(RefCell::new(Ok(())));
    let bootstrap = Rc::new(RefCell::new(SurfaceBootstrap::new(receiver)));
    let timer_shell = shell.clone();
    let timer_result = result.clone();
    let timer_app = app.clone();
    let keys = hotkeys.clone();
    let timer_delegate = delegate.clone();
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let Ok(mut shell) = timer_shell.try_borrow_mut() else {
            return;
        };
        if timer_delegate.take_quit() {
            if let Some(native) = shell.as_mut() {
                let now = native.now();
                native.runtime.on_event(SurfaceEvent::Terminate, now);
            } else {
                timer_app.stop(None);
                return;
            }
        }
        if shell.is_none() {
            match bootstrap.borrow_mut().poll() {
                Ok(BootstrapUpdate::Pending) => {}
                Ok(BootstrapUpdate::Start(host)) => match MacShell::new(mtm, host, &keys) {
                    Ok(native) => {
                        *shell = Some(native);
                    }
                    Err(error) => {
                        let error = error.to_string();
                        show_startup_error(&error, mtm);
                        *timer_result.borrow_mut() = Err(error);
                        timer_app.stop(None);
                    }
                },
                Ok(BootstrapUpdate::Reentered) => timer_app.stop(None),
                Err(error) => {
                    show_startup_error(&error, mtm);
                    *timer_result.borrow_mut() = Err(error);
                    timer_app.stop(None);
                }
            }
        }
        if let Some(native) = shell.as_mut() {
            if timer_delegate.take_open() {
                let now = native.now();
                native
                    .runtime
                    .on_event(SurfaceEvent::Menu(MenuAction::OpenNotes), now);
            }
            native.pump();
            if native.runtime.should_exit() {
                timer_app.stop(None);
            }
        }
    });
    // SAFETY: NSTimer invokes this block only from the main AppKit run loop.
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(PUMP_INTERVAL, true, &block)
    };
    app.run();
    timer.invalidate();
    app.setDelegate(None);
    // Tear down native/global resources before returning to runtime shutdown.
    shell.borrow_mut().take();
    result.borrow().clone()
}

fn show_startup_error(error: &str, mtm: MainThreadMarker) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&objc2_foundation::NSString::from_str(
        "FlyOnTheWall could not start",
    ));
    alert.setInformativeText(&objc2_foundation::NSString::from_str(error));
    let _ = alert.addButtonWithTitle(&objc2_foundation::NSString::from_str("Close"));
    let _ = alert.runModal();
}

/// Render and inspect one owned nonactivating indicator, then hide it. This
/// fixture creates no tray/global hotkeys and does not launch the product.
///
/// # Errors
/// An off-main-thread call, view rendering or PNG output failure.
pub fn probe_indicator_frame(
    view: &crate::PillView,
    path: &std::path::Path,
) -> Result<crate::probe::IndicatorFrameProbe, String> {
    let mtm = MainThreadMarker::new().ok_or_else(|| ShellError::NotMainThread.to_string())?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let mut pill = Pill::new(mtm);
    pill.render(Some(view), mtm);
    let result = pill.capture_frame(path, mtm);
    pill.render(None, mtm);
    result
}

/// Exercise only the owned consent panel and injected host. No tray, global
/// hotkeys or product launch is created. The caller must inject device-free
/// dependencies: the final acknowledged response is delivered to that host.
///
/// # Errors
/// An off-main-thread call or missing consent prompt.
pub fn probe_consent_controls<H: SurfaceHost>(
    host: &mut H,
    now: Monotonic,
) -> Result<crate::probe::ConsentControlProbe, String> {
    let mtm = MainThreadMarker::new().ok_or_else(|| ShellError::NotMainThread.to_string())?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    if host.view().prompt.is_none() {
        return Err("owned host did not offer a consent prompt".into());
    }
    let mut prompt = Prompt::new(mtm);
    prompt.render(host.view().prompt.as_ref(), mtm);
    let nonactivating = prompt.nonactivating();
    let on_screen = prompt.is_on_a_screen(mtm);
    let unchecked_start_disabled = !prompt.start_is_enabled();
    prompt.press(ProbePress::Start);
    let unchecked_click_blocked = prompt.take_response().is_none();
    prompt.press(ProbePress::Acknowledge);
    let acknowledged = prompt.take_acknowledgement();
    if let Some(value) = acknowledged {
        host.on_event(SurfaceEvent::Acknowledge(value), now);
    }
    prompt.render(host.view().prompt.as_ref(), mtm);
    let acknowledged_start_enabled = prompt.start_is_enabled();
    prompt.press(ProbePress::Start);
    let choice = prompt.take_response();
    let start_response_acknowledged =
        matches!(choice, Some(PromptChoice::Start { acknowledged: true }));
    if let Some(choice) = choice {
        host.on_event(SurfaceEvent::Prompt(choice), now);
    }
    let prompt_cleared = host.view().prompt.is_none();
    prompt.render(None, mtm);
    Ok(crate::probe::ConsentControlProbe {
        nonactivating,
        on_screen,
        unchecked_start_disabled,
        unchecked_click_blocked,
        acknowledgement_delivered: acknowledged == Some(true),
        acknowledged_start_enabled,
        start_response_acknowledged,
        prompt_cleared,
    })
}

/// Invoke the owned delegate's real Objective-C selectors without installing
/// global controls or launching the product.
///
/// # Errors
/// If called off the AppKit main thread.
pub fn probe_application_events() -> Result<crate::probe::ApplicationEventProbe, String> {
    use objc2::msg_send;
    let mtm = MainThreadMarker::new().ok_or_else(|| ShellError::NotMainThread.to_string())?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = app_delegate::AppDelegate::new(mtm);
    // SAFETY: this owned main-thread delegate implements both exact signatures.
    let default_reopen: bool = unsafe {
        msg_send![&*delegate, applicationShouldHandleReopen: &*app, hasVisibleWindows: false]
    };
    let opened = delegate.take_open();
    let open_once = !delegate.take_open();
    // SAFETY: the callback queues a flag only; no termination is performed here.
    let reply: objc2_app_kit::NSApplicationTerminateReply =
        unsafe { msg_send![&*delegate, applicationShouldTerminate: &*app] };
    let quit = delegate.take_quit();
    let quit_once = !delegate.take_quit();
    Ok(crate::probe::ApplicationEventProbe {
        reopen_queued_once: !default_reopen && opened && open_once,
        quit_queued_once: quit && quit_once,
        immediate_termination_cancelled: reply
            == objc2_app_kit::NSApplicationTerminateReply::TerminateCancel,
    })
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
            "Recording laws that may apply to this call:\n  \
             • California: every participant must consent. \
             Recording without that consent is a CRIMINAL offence.\n    \
             Cal. Penal Code § 632 (https://leginfo.legislature.ca.gov/faces/\
             codes_displaySection.xhtml?lawCode=PEN&sectionNum=632)\n    \
             Punishable by a fine, imprisonment, or both (§ 632(a)).\n\
             This is not legal advice.",
            true,
        )
}

struct MacShell<H: SurfaceHost> {
    runtime: H,
    tray: Tray,
    pill: Pill,
    prompt: Prompt,
    hotkeys: HotkeyRegistrar,
    origin: Instant,
    mtm: MainThreadMarker,
    frames: FrameAdmission,
}

impl<H: SurfaceHost> MacShell<H> {
    fn new(mtm: MainThreadMarker, host: H, hotkeys: &HotkeyMap) -> Result<Self, ShellError> {
        let registrar = HotkeyRegistrar::register(hotkeys)?;
        let runtime = host;
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
            frames: FrameAdmission::default(),
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
                self.runtime.on_event(SurfaceEvent::Chord(chord), now);
            }
        }

        // Menu clicks.
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(action) = MenuAction::from_id(&event.id.0) {
                self.runtime.on_event(SurfaceEvent::Menu(action), now);
            }
        }

        // Tray clicks. We take no action on them -- the menu opens by itself
        // -- but the channel is unbounded and would otherwise grow for the
        // life of the process.
        while TrayIconEvent::receiver().try_recv().is_ok() {}

        // The pill's Stop button.
        if self.pill.take_stop_request() {
            self.runtime.on_event(SurfaceEvent::Stop, now);
        }

        // The prompt. The acknowledgement is applied first: if a user manages
        // to tick the box and press Start inside one 50 ms poll, the tick was
        // still first in wall-clock order and has to be honoured that way
        // (CON-05).
        if let Some(acknowledged) = self.prompt.take_acknowledgement() {
            self.runtime
                .on_event(SurfaceEvent::Acknowledge(acknowledged), now);
        }
        if let Some(choice) = self.prompt.take_response() {
            self.runtime.on_event(SurfaceEvent::Prompt(choice), now);
        }

        self.runtime.tick(now);
        self.render();
    }

    fn render(&mut self) {
        let Self {
            runtime,
            tray,
            pill,
            prompt,
            mtm,
            frames,
            ..
        } = self;
        let _ = render_surface(runtime, |view| {
            tray.render(&view.tray, &view.menu);
            pill.render(view.pill.as_ref(), *mtm);
            prompt.render(view.prompt.as_ref(), *mtm);
            if frames.observe(view.pill.as_ref(), pill.visible_on_screen(*mtm)) {
                Ok(())
            } else {
                Err(())
            }
        });
    }
}
