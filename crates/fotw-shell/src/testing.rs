//! Fakes for driving the shell without a window server.
//!
//! Shipped in the crate rather than in `tests/` so the daemon can reuse them,
//! matching `fotw_audio::testing`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::runtime::ShellHost;
use crate::view::Level;

/// Everything a [`ShellHost`] was asked to do, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostCall {
    /// [`ShellHost::start_capture`] was called and succeeded.
    StartCapture,
    /// [`ShellHost::start_capture`] was called and was told to fail.
    StartCaptureFailed,
    /// [`ShellHost::stop_capture`].
    StopCapture,
    /// [`ShellHost::set_ticking`].
    SetTicking(bool),
    /// [`ShellHost::open_notes`].
    OpenNotes,
    /// [`ShellHost::open_disclosure_kit`].
    OpenDisclosureKit,
    /// [`ShellHost::open_settings`].
    OpenSettings,
    /// [`ShellHost::open_about`].
    OpenAbout,
    /// [`ShellHost::quit`].
    Quit,
}

/// A [`ShellHost`] that records what it was asked to do.
///
/// The call log is behind an `Rc<RefCell<_>>` so a test can keep reading it
/// after the host has been moved into a
/// [`ShellRuntime`](crate::ShellRuntime).
#[derive(Clone, Debug, Default)]
pub struct FakeHost {
    calls: Rc<RefCell<Vec<HostCall>>>,
    level: Rc<RefCell<Level>>,
    fail_start_with: Rc<RefCell<Option<String>>>,
}

impl FakeHost {
    /// A host that succeeds at everything and reports silence.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle to the call log that survives the host being moved.
    #[must_use]
    pub fn log(&self) -> Rc<RefCell<Vec<HostCall>>> {
        Rc::clone(&self.calls)
    }

    /// Every call so far.
    #[must_use]
    pub fn calls(&self) -> Vec<HostCall> {
        self.calls.borrow().clone()
    }

    /// How many times a given call was made.
    #[must_use]
    pub fn count(&self, call: &HostCall) -> usize {
        self.calls.borrow().iter().filter(|c| *c == call).count()
    }

    /// Forget every recorded call.
    pub fn clear(&self) {
        self.calls.borrow_mut().clear();
    }

    /// Make the next and every subsequent `start_capture` fail.
    pub fn fail_start(&self, reason: impl Into<String>) {
        *self.fail_start_with.borrow_mut() = Some(reason.into());
    }

    /// Make `start_capture` succeed again.
    pub fn allow_start(&self) {
        *self.fail_start_with.borrow_mut() = None;
    }

    /// Set the level the host reports on the next tick.
    pub fn set_level(&self, level: Level) {
        *self.level.borrow_mut() = level;
    }

    fn push(&self, call: HostCall) {
        self.calls.borrow_mut().push(call);
    }
}

impl ShellHost for FakeHost {
    fn start_capture(&mut self) -> Result<(), String> {
        if let Some(reason) = self.fail_start_with.borrow().clone() {
            self.push(HostCall::StartCaptureFailed);
            return Err(reason);
        }
        self.push(HostCall::StartCapture);
        Ok(())
    }

    fn stop_capture(&mut self) {
        self.push(HostCall::StopCapture);
    }

    fn quit(&mut self) {
        self.push(HostCall::Quit);
    }

    fn level(&mut self) -> Level {
        *self.level.borrow()
    }

    fn set_ticking(&mut self, ticking: bool) {
        self.push(HostCall::SetTicking(ticking));
    }

    fn open_notes(&mut self) {
        self.push(HostCall::OpenNotes);
    }

    fn open_disclosure_kit(&mut self) {
        self.push(HostCall::OpenDisclosureKit);
    }

    fn open_settings(&mut self) {
        self.push(HostCall::OpenSettings);
    }

    fn open_about(&mut self) {
        self.push(HostCall::OpenAbout);
    }
}

/// A deterministic xorshift generator.
///
/// The invariant tests drive tens of thousands of random input sequences and
/// must reproduce byte-for-byte when one fails, so they cannot use a seeded
/// third-party RNG that might change its stream between versions.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator with a fixed seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x2545_F491_4F6C_DD1D
        } else {
            seed
        })
    }

    /// The next value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `0..n`.
    ///
    /// # Panics
    ///
    /// If `n` is zero.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "bound must be positive");
        self.next_u64() % n
    }
}
