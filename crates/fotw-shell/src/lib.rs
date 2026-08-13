//! `fotw-shell` — FlyOnTheWall's native macOS shell, in pure Rust.
//!
//! No Swift, no Xcode project, no `.xib`: an `NSPanel` subclass built with
//! `objc2`, a menu-bar item through `tray-icon`, and Carbon global hotkeys
//! through `global-hotkey` (docs/REQUIREMENTS.md 5.5).
//!
//! # What is in here
//!
//! - [`ShellCore`] — the state machine. Idle → recording → finishing →
//!   finished, what the pill shows, what the menu offers. Plain Rust, no
//!   AppKit in any signature, no clock of its own.
//! - [`ShellRuntime`] — the core wired to a [`ShellHost`], which is what the
//!   daemon implements.
//! - [`hotkey`] — chords, and the rule that keeps a media key from silently
//!   switching `global-hotkey` onto its Accessibility-gated `CGEventTap` path.
//! - [`platform`] — the AppKit renderer on macOS, a typed refusal elsewhere.
//!
//! # Why the state machine is separate
//!
//! `NSApplication::run()` never returns and must own the main thread, so
//! anything behind it cannot be exercised by a test. GitHub's runners have no
//! window server at all. Splitting the decisions out from the drawing is the
//! only way any of this has coverage; the AppKit layer is deliberately thin
//! enough to review by eye, and what it cannot prove is a line in
//! `crates/fotw-shell/QA.md` rather than a test that passes vacuously.
//!
//! # CON-02
//!
//! The recording indicator is not suppressible. There is no build feature, no
//! preference and no argument that takes it down, and this crate has no
//! `[features]` table at all so that a future one cannot be mistaken for
//! sanctioned. See docs/REQUIREMENTS.md 11.2, and *Chamberlain v. Granola*
//! for why "no hide-indicator setting" is a legal position and not a taste.
//!
//! # CON-01
//!
//! Detection arms; a person starts. [`ShellInput::MeetingDetected`] can only
//! raise a [`PromptView`], every way of starting names a human
//! ([`StartOrigin`] has no automatic variant), and
//! `tests/con01_detection_arms_only.rs` is the executable form of both.
//!
//! That prompt is drawn by `platform::macos::prompt` and reaches the screen
//! through [`ShellHost::poll_detection`] — the only channel a detector has
//! into a running shell, and one that can say "a meeting seems to be
//! happening" and nothing else. `cargo run -p fotw-shell --example
//! prompt_preview` puts it on screen without a meeting.
//!
//! ```
//! use fotw_shell::{
//!     DetectedMeeting, Monotonic, PromptChoice, ShellCore, ShellEffect, ShellInput, StartOrigin,
//! };
//!
//! let mut shell = ShellCore::new();
//! assert!(shell.view().pill.is_none());
//!
//! // Detection arms. It does not record.
//! let effects = shell.handle(ShellInput::MeetingDetected {
//!     at: Monotonic::ZERO,
//!     meeting: DetectedMeeting::new("us.zoom.xos", "Zoom", "Zoom is using the microphone"),
//! });
//! assert!(!effects.contains(&ShellEffect::StartCapture));
//! assert!(shell.view().prompt.is_some());
//! assert!(shell.view().pill.is_none());
//!
//! // A person presses the button.
//! shell.handle(ShellInput::PromptResponse {
//!     at: Monotonic::ZERO,
//!     choice: PromptChoice::Start { acknowledged: true },
//! });
//! shell.handle(ShellInput::Tick { now: Monotonic::from_secs(754) });
//!
//! let pill = shell.view().pill.expect("recording shows an indicator");
//! assert_eq!(pill.elapsed_label, "12:34");
//!
//! // Stopping does not take the indicator down: the session is still being
//! // written to disk.
//! shell.handle(ShellInput::StopRequested);
//! assert!(shell.view().pill.is_some());
//!
//! // And the same session could have been started from the menu instead.
//! let mut other = ShellCore::new();
//! other.handle(ShellInput::Start { at: Monotonic::ZERO, origin: StartOrigin::Menu });
//! assert!(other.capture_is_live());
//! ```

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
// This crate's prose is mostly Apple framework and API names -- AppKit,
// CGEventTap, NSStatusItem, LSUIElement. Backticking every one of them makes
// the "why" comments, which are the load-bearing part of this crate, much
// harder to read.
#![allow(clippy::doc_markdown)]

pub mod clock;
mod error;
pub mod hotkey;
pub mod icon;
pub mod platform;
pub mod probe;
pub mod prompt;
mod runtime;
mod state;
pub mod testing;
mod view;

pub use crate::clock::{Monotonic, format_elapsed};
pub use crate::error::ShellError;
pub use crate::hotkey::{Chord, HotkeyAction, HotkeyError, HotkeyMap, Key, MediaKey, Modifiers};
pub use crate::platform::run;
pub use crate::prompt::{DetectedMeeting, DetectionUpdate, PromptChoice, StartOrigin};
pub use crate::runtime::{ShellHost, ShellRuntime};
pub use crate::state::{
    FINISHED_LINGER, METER_SEGMENTS, Phase, ShellCore, ShellEffect, ShellInput,
};
pub use crate::view::{
    Level, MenuAction, MenuButton, MenuModel, PillView, PromptView, ShellView, Tone, TrayState,
    TrayView,
};
