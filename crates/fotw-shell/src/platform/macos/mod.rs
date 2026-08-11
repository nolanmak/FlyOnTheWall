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
//! Nothing in this module has automated coverage: GitHub's macOS runners have
//! no window server, and the properties that matter (does the pill stay above
//! a full-screen Zoom, does clicking Stop steal focus, is it excluded from a
//! screen share) are observable only on a real desktop with a real meeting on
//! it. Those are `crates/fotw-shell/QA.md` line items, per release. Claiming
//! otherwise would be worse than claiming nothing.

mod hotkeys;
mod pill;
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
use crate::platform::macos::tray::Tray;
use crate::probe::ShellProbe;
use crate::runtime::{ShellHost, ShellRuntime};
use crate::state::ShellCore;
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
pub fn probe() -> Result<ShellProbe, ShellError> {
    let mtm = MainThreadMarker::new().ok_or(ShellError::NotMainThread)?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let hotkeys = HotkeyMap::defaults();
    let registrar = HotkeyRegistrar::register(&hotkeys)?;
    let tray = Tray::new(&ShellCore::new().view().menu)?;
    let pill = Pill::new(mtm);

    let mut probe = ShellProbe {
        activation_policy: app.activationPolicy().0,
        panel_style_mask: 0,
        panel_is_nonactivating: false,
        panel_can_become_key: false,
        panel_can_become_main: true,
        panel_level: 0,
        panel_sharing_type: usize::MAX,
        panel_collection_behavior: 0,
        status_item_retained: true,
        hotkeys_registered: hotkeys.len(),
    };
    pill.probe_into(&mut probe);

    drop(tray);
    drop(registrar);
    Ok(probe)
}

struct MacShell<H: ShellHost> {
    runtime: ShellRuntime<H>,
    tray: Tray,
    pill: Pill,
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
        Ok(Self {
            runtime,
            tray,
            pill,
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

        self.runtime.tick(now);
        self.render();
    }

    fn render(&mut self) {
        let view = self.runtime.view();
        self.tray.render(&view.tray, &view.menu);
        self.pill.render(view.pill.as_ref(), self.mtm);
    }
}
