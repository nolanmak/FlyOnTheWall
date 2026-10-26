//! Capture/finalization ownership independent of the live device slot.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Shutdown observation. Idle alone does not prove finalization has completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownStatus {
    /// New Start admission has been permanently closed for this recorder.
    pub closing: bool,
    /// A capture/preparation/native cleanup slot remains occupied.
    pub active: bool,
    /// Accepted attempts still configuring, capturing or persisting audio.
    pub outstanding: usize,
    /// At least one completed capture did not confirm a library save.
    pub save_failed: bool,
}
impl ShutdownStatus {
    /// No accepted capture or persistence owner remains, and new starts are shut.
    #[must_use]
    pub const fn drained(self) -> bool {
        self.closing && !self.active && self.outstanding == 0
    }
}
#[derive(Default)]
pub(crate) struct Lifecycle {
    pub(crate) closing: AtomicBool,
    state: Mutex<WorkState>,
}
#[derive(Default)]
struct WorkState {
    outstanding: usize,
    save_failed: bool,
}
impl Lifecycle {
    pub(crate) fn begin(self: &Arc<Self>) -> WorkLease {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outstanding += 1;
        WorkLease {
            lifecycle: self.clone(),
            save_required: false,
        }
    }
    pub(crate) fn status(&self, active: bool) -> ShutdownStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ShutdownStatus {
            closing: self.closing.load(Ordering::Acquire),
            active,
            outstanding: state.outstanding,
            save_failed: state.save_failed,
        }
    }
}
pub(crate) struct WorkLease {
    lifecycle: Arc<Lifecycle>,
    save_required: bool,
}
impl WorkLease {
    pub(crate) fn require_save(&mut self) {
        self.save_required = true;
    }
    pub(crate) fn saved(&mut self) {
        self.save_required = false;
    }
}
impl Drop for WorkLease {
    fn drop(&mut self) {
        let mut state = self
            .lifecycle
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.save_failed |= self.save_required;
        state.outstanding -= 1;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_and_cancelled_owners_release_but_unconfirmed_saves_remain_visible() {
        let lifecycle = Arc::new(Lifecycle::default());
        let cancelled = lifecycle.begin();
        let mut saved = lifecycle.begin();
        saved.require_save();
        assert_eq!(lifecycle.status(false).outstanding, 2);
        saved.saved();
        drop(saved);
        drop(cancelled);
        assert!(!lifecycle.status(false).save_failed);
        let mut failed = lifecycle.begin();
        failed.require_save();
        drop(failed);
        assert!(lifecycle.status(false).save_failed);
        assert_eq!(lifecycle.status(false).outstanding, 0);
        lifecycle.closing.store(true, Ordering::Release);
        assert!(lifecycle.status(false).drained());
        assert!(!lifecycle.status(true).drained());
    }
}
