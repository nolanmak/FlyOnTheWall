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
//! ```
//! use fotw_shell::{Monotonic, ShellCore, ShellInput};
//!
//! let mut shell = ShellCore::new();
//! assert!(shell.view().pill.is_none());
//!
//! shell.handle(ShellInput::Start { at: Monotonic::ZERO });
//! shell.handle(ShellInput::Tick { now: Monotonic::from_secs(754) });
//!
//! let pill = shell.view().pill.expect("recording shows an indicator");
//! assert_eq!(pill.elapsed_label, "12:34");
//!
//! // Stopping does not take the indicator down: the session is still being
//! // written to disk.
//! shell.handle(ShellInput::StopRequested);
//! assert!(shell.view().pill.is_some());
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
mod runtime;
mod state;
pub mod testing;
mod view;

pub use crate::clock::{Monotonic, format_elapsed};
pub use crate::error::ShellError;
pub use crate::hotkey::{Chord, HotkeyAction, HotkeyError, HotkeyMap, Key, MediaKey, Modifiers};
pub use crate::platform::run;
pub use crate::runtime::{ShellHost, ShellRuntime};
pub use crate::state::{
    FINISHED_LINGER, METER_SEGMENTS, Phase, ShellCore, ShellEffect, ShellInput,
};
pub use crate::view::{
    Level, MenuAction, MenuButton, MenuModel, PillView, ShellView, Tone, TrayState, TrayView,
};
