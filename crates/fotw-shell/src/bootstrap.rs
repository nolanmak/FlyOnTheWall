//! Lazy native creation: existing-daemon reentry never creates global controls.
use std::sync::mpsc::{Receiver, TryRecvError};

/// Result of polling background daemon initialization.
pub enum BootstrapUpdate<H> {
    /// The daemon is still initializing; do not create native controls yet.
    Pending,
    /// This process owns the daemon and may create its one native shell.
    Start(H),
    /// The existing UI was reopened; this process should exit without a shell.
    Reentered,
}
/// Poll-only bootstrap transport; no DB, credential or recorder work on AppKit.
pub struct SurfaceBootstrap<H> {
    receiver: Receiver<Result<Option<H>, String>>,
    resolved: bool,
}
impl<H> SurfaceBootstrap<H> {
    /// Connect the background initialization result.
    #[must_use]
    pub const fn new(receiver: Receiver<Result<Option<H>, String>>) -> Self {
        Self {
            receiver,
            resolved: false,
        }
    }
    /// Poll without waiting. A host is delivered at most once.
    ///
    /// # Errors
    /// Startup failed or the background initializer disappeared before replying.
    pub fn poll(&mut self) -> Result<BootstrapUpdate<H>, String> {
        if self.resolved {
            return Ok(BootstrapUpdate::Pending);
        }
        match self.receiver.try_recv() {
            Ok(Ok(Some(host))) => {
                self.resolved = true;
                Ok(BootstrapUpdate::Start(host))
            }
            Ok(Ok(None)) => {
                self.resolved = true;
                Ok(BootstrapUpdate::Reentered)
            }
            Ok(Err(error)) => {
                self.resolved = true;
                Err(error)
            }
            Err(TryRecvError::Empty) => Ok(BootstrapUpdate::Pending),
            Err(TryRecvError::Disconnected) => {
                self.resolved = true;
                Err("daemon initialization stopped before the native shell was ready".into())
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    #[test]
    fn controls_are_not_requested_before_ownership_or_for_existing_daemon_reentry() {
        let (sender, receiver) = mpsc::sync_channel::<Result<Option<u8>, String>>(1);
        let mut bootstrap = SurfaceBootstrap::new(receiver);
        assert!(matches!(bootstrap.poll(), Ok(BootstrapUpdate::Pending)));
        sender.send(Ok(None)).unwrap();
        assert!(matches!(bootstrap.poll(), Ok(BootstrapUpdate::Reentered)));
        assert!(matches!(bootstrap.poll(), Ok(BootstrapUpdate::Pending)));
    }
    #[test]
    fn owner_is_delivered_once_and_startup_errors_do_not_request_controls() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut bootstrap = SurfaceBootstrap::new(receiver);
        sender.send(Ok(Some(7))).unwrap();
        assert!(matches!(bootstrap.poll(), Ok(BootstrapUpdate::Start(7))));
        assert!(matches!(bootstrap.poll(), Ok(BootstrapUpdate::Pending)));
        let (sender, receiver) = mpsc::sync_channel::<Result<Option<u8>, String>>(1);
        let mut bootstrap = SurfaceBootstrap::new(receiver);
        sender.send(Err("owned startup failure".into())).unwrap();
        assert_eq!(bootstrap.poll().err().unwrap(), "owned startup failure");
        let (sender, receiver) = mpsc::sync_channel::<Result<Option<u8>, String>>(1);
        drop(sender);
        assert!(SurfaceBootstrap::new(receiver).poll().is_err());
    }
}
