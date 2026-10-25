//! Explicit control-thread teardown. Retain native lifetimes when release is
//! unconfirmed; never free callback state while its registration may still run.
use crate::{CleanupState, TapError};
use std::mem::ManuallyDrop;

pub(super) trait CaptureAssets {
    type Proc: Copy;
    fn register(&mut self) -> Result<Self::Proc, String>;
    fn start(&mut self, proc: Self::Proc) -> Result<(), String>;
    fn stop(&mut self, proc: Self::Proc) -> Result<(), String>;
    fn destroy_proc(&mut self, proc: Self::Proc) -> Result<(), String>;
    fn drain(&mut self);
    fn release_device(&mut self) -> Result<(), String>;
    fn release_tap(&mut self) -> Result<(), String>;
    fn retain(&mut self, _failure: &Failure) {}
}
#[derive(Clone)]
pub(super) struct Failure {
    pub state: CleanupState,
    pub message: String,
}
impl Failure {
    pub fn from_error(error: &TapError) -> Option<Self> {
        match error {
            TapError::Cleanup { state, message } if !state.resources_released() => Some(Self {
                state: *state,
                message: message.clone(),
            }),
            _ => None,
        }
    }
    pub fn error(&self) -> TapError {
        TapError::Cleanup {
            state: self.state,
            message: self.message.clone(),
        }
    }
}
pub(super) struct CaptureOwner<A: CaptureAssets> {
    assets: Option<ManuallyDrop<A>>,
    proc: Option<A::Proc>,
    start_attempted: bool,
    failure: Option<Failure>,
}
impl<A: CaptureAssets> CaptureOwner<A> {
    fn owned(assets: A) -> Self {
        Self {
            assets: Some(ManuallyDrop::new(assets)),
            proc: None,
            start_attempted: false,
            failure: None,
        }
    }
    /// Own partial native resources before running fallible setup. Return
    /// cleanup failure explicitly; Drop alone cannot report it to recovery.
    pub fn prepare<T>(
        assets: A,
        prepare: impl FnOnce(&mut A) -> Result<T, TapError>,
    ) -> Result<(Self, T), TapError> {
        let mut owner = Self::owned(assets);
        let value = match prepare(owner.assets.as_mut().expect("owned assets")) {
            Ok(value) => value,
            Err(error) => return Err(owner.failed_setup(error)),
        };
        Ok((owner, value))
    }
    #[cfg(test)]
    pub fn start(assets: A) -> Result<Self, TapError> {
        Self::owned(assets).start_prepared()
    }
    pub fn start_prepared(mut self) -> Result<Self, TapError> {
        let owner = &mut self;
        let setup = (|| {
            let assets = owner.assets.as_mut().expect("owned assets");
            let proc = assets.register()?;
            owner.proc = Some(proc);
            owner.start_attempted = true;
            assets.start(proc)
        })();
        if let Err(setup) = setup {
            return Err(owner.failed_setup(TapError::platform(setup)));
        }
        Ok(self)
    }
    fn failed_setup(&mut self, setup: TapError) -> TapError {
        match self.close() {
            Err(TapError::Cleanup { state, message }) => TapError::Cleanup {
                state,
                message: format!("{setup}; {message}"),
            },
            _ => setup,
        }
    }
    pub fn close(&mut self) -> Result<(), TapError> {
        self.close_observed(&mut || {})
    }
    pub fn close_observed(&mut self, stopped: &mut dyn FnMut()) -> Result<(), TapError> {
        let Some(mut assets) = self.assets.take() else {
            return self
                .failure
                .as_ref()
                .map_or(Ok(()), |failure| Err(failure.error()));
        };
        // ManuallyDrop also retains the callback during an unexpected unwind.
        let mut errors = Vec::new();
        let mut capture_stopped = !self.start_attempted;
        let mut unregistered = self.proc.is_none();
        let mut notified = false;
        if let Some(proc) = self.proc {
            if self.start_attempted {
                match assets.stop(proc) {
                    Ok(()) => {
                        capture_stopped = true;
                        stopped();
                        notified = true;
                    }
                    Err(error) => errors.push(error),
                }
            }
            match assets.destroy_proc(proc) {
                Ok(()) => {
                    unregistered = true;
                    capture_stopped = true;
                }
                Err(error) => errors.push(error),
            }
        }
        if capture_stopped && !notified {
            stopped();
        }
        let mut released = false;
        if unregistered {
            assets.drain();
            match assets.release_device() {
                Ok(()) => match assets.release_tap() {
                    Ok(()) => released = true,
                    Err(error) => errors.push(error),
                },
                Err(error) => errors.push(error),
            }
        }
        let state = if released {
            CleanupState::Released
        } else if capture_stopped {
            CleanupState::Stopped
        } else {
            CleanupState::Unconfirmed
        };
        if released {
            // All native references have been removed and queued callbacks drained.
            // SAFETY: this is the unique owned value, consumed exactly once.
            unsafe { ManuallyDrop::drop(&mut assets) };
        }
        // On failure intentionally retain the complete bundle until process exit.
        // Callers must prevent reopening this unconfirmed source; its callbacks
        // can no longer be allowed to observe freed Rust capture state.
        if errors.is_empty() {
            return Ok(());
        }
        let failure = Failure {
            state,
            message: errors.join("; "),
        };
        if !released {
            assets.retain(&failure);
            self.failure = Some(failure.clone());
        }
        Err(failure.error())
    }
}
impl<A: CaptureAssets> Drop for CaptureOwner<A> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, Weak};
    #[derive(Default)]
    struct Native {
        events: Mutex<Vec<&'static str>>,
        registration: Mutex<Option<Arc<()>>>,
        retained: Mutex<Vec<CleanupState>>,
        fail: Vec<&'static str>,
    }
    impl Native {
        fn call(&self, operation: &'static str) -> Result<(), String> {
            self.events.lock().unwrap().push(operation);
            if self.fail.contains(&operation) {
                Err(format!("injected {operation} failure"))
            } else {
                Ok(())
            }
        }
    }
    struct Assets {
        native: Arc<Native>,
        callback: Arc<()>,
        device: bool,
        queue: bool,
    }
    impl Drop for Assets {
        fn drop(&mut self) {
            self.native.events.lock().unwrap().push("drop_assets");
        }
    }
    impl CaptureAssets for Assets {
        type Proc = u32;
        fn register(&mut self) -> Result<u32, String> {
            self.native.call("register")?;
            *self.native.registration.lock().unwrap() = Some(self.callback.clone());
            Ok(17)
        }
        fn start(&mut self, _: u32) -> Result<(), String> {
            self.native.call("start")
        }
        fn stop(&mut self, _: u32) -> Result<(), String> {
            self.native.call("stop")
        }
        fn destroy_proc(&mut self, _: u32) -> Result<(), String> {
            self.native.call("destroy_proc")?;
            self.native.registration.lock().unwrap().take();
            Ok(())
        }
        fn drain(&mut self) {
            if self.queue {
                self.native.call("drain").unwrap();
            }
        }
        fn release_device(&mut self) -> Result<(), String> {
            if self.device {
                self.native.call("destroy_device")
            } else {
                Ok(())
            }
        }
        fn release_tap(&mut self) -> Result<(), String> {
            self.native.call("destroy_tap")
        }
        fn retain(&mut self, failure: &Failure) {
            self.native.retained.lock().unwrap().push(failure.state);
        }
    }
    fn assets(fail: Option<&'static str>) -> (Assets, Arc<Native>, Weak<()>) {
        let native = Arc::new(Native {
            fail: fail.into_iter().collect(),
            ..Default::default()
        });
        let callback = Arc::new(());
        let weak = Arc::downgrade(&callback);
        (
            Assets {
                native: native.clone(),
                callback,
                device: true,
                queue: true,
            },
            native,
            weak,
        )
    }
    #[test]
    fn normal_close_unregisters_before_releasing_callback_queue_and_device() {
        let (assets, native, callback) = assets(None);
        let mut owner = CaptureOwner::start(assets).unwrap();
        owner.close().unwrap();
        assert_eq!(
            *native.events.lock().unwrap(),
            [
                "register",
                "start",
                "stop",
                "destroy_proc",
                "drain",
                "destroy_device",
                "destroy_tap",
                "drop_assets"
            ]
        );
        assert!(
            callback.upgrade().is_none(),
            "the native registration must no longer retain its callback"
        );
        owner.close().unwrap();
        assert_eq!(
            native
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == "destroy_proc")
                .count(),
            1
        );
    }
    #[test]
    fn failed_start_still_unregisters_the_successfully_created_callback() {
        let (assets, native, callback) = assets(Some("start"));
        assert!(CaptureOwner::start(assets).is_err());
        assert!(
            native.events.lock().unwrap().contains(&"destroy_proc"),
            "a failed start must not leak its registration"
        );
        assert!(callback.upgrade().is_none());
    }
    #[test]
    fn registration_failure_releases_dependencies_without_stopping_an_unknown_proc() {
        let (assets, native, callback) = assets(Some("register"));
        assert!(CaptureOwner::start(assets).is_err());
        assert_eq!(
            *native.events.lock().unwrap(),
            [
                "register",
                "drain",
                "destroy_device",
                "destroy_tap",
                "drop_assets"
            ]
        );
        assert!(callback.upgrade().is_none());
    }
    #[test]
    fn stop_error_with_successful_unregister_releases_all_resources() {
        let (assets, native, callback) = assets(Some("stop"));
        let mut owner = CaptureOwner::start(assets).unwrap();
        let error = owner
            .close_observed(&mut || native.events.lock().unwrap().push("stopped"))
            .unwrap_err();
        assert_eq!(error.cleanup_state(), Some(CleanupState::Released));
        assert_eq!(
            *native.events.lock().unwrap(),
            [
                "register",
                "start",
                "stop",
                "destroy_proc",
                "stopped",
                "drain",
                "destroy_device",
                "destroy_tap",
                "drop_assets"
            ]
        );
        assert!(callback.upgrade().is_none());
        owner.close().unwrap();
    }
    #[test]
    fn failed_unregister_retains_callback_queue_and_dependencies_without_repeating_cleanup() {
        let (assets, native, callback) = assets(Some("destroy_proc"));
        let mut owner = CaptureOwner::start(assets).unwrap();
        let error = owner
            .close_observed(&mut || native.events.lock().unwrap().push("stopped"))
            .unwrap_err();
        assert_eq!(error.cleanup_state(), Some(CleanupState::Stopped));
        assert_eq!(
            *native.events.lock().unwrap(),
            ["register", "start", "stop", "stopped", "destroy_proc"]
        );
        assert!(callback.upgrade().is_some());
        let before = native.events.lock().unwrap().clone();
        assert_eq!(
            owner.close().unwrap_err().cleanup_state(),
            Some(CleanupState::Stopped)
        );
        drop(owner);
        assert_eq!(*native.events.lock().unwrap(), before);
    }
    #[test]
    fn failed_stop_and_unregister_never_claim_capture_stopped() {
        let (mut assets, _, callback) = assets(None);
        assets.native = Arc::new(Native {
            fail: vec!["stop", "destroy_proc"],
            ..Default::default()
        });
        let native = assets.native.clone();
        let mut owner = CaptureOwner::start(assets).unwrap();
        let error = owner
            .close_observed(&mut || panic!("capture stop was not confirmed"))
            .unwrap_err();
        assert_eq!(error.cleanup_state(), Some(CleanupState::Unconfirmed));
        assert_eq!(
            *native.events.lock().unwrap(),
            ["register", "start", "stop", "destroy_proc"]
        );
        assert!(callback.upgrade().is_some());
    }
    #[test]
    fn failed_dependency_release_retains_bundle_and_respects_dependency_order() {
        for operation in ["destroy_device", "destroy_tap"] {
            let (assets, native, callback) = assets(Some(operation));
            let mut owner = CaptureOwner::start(assets).unwrap();
            assert_eq!(
                owner.close().unwrap_err().cleanup_state(),
                Some(CleanupState::Stopped)
            );
            assert!(
                callback.upgrade().is_some(),
                "unreleased bundles retain Rust lifetimes"
            );
            let events = native.events.lock().unwrap();
            assert!(events.contains(&"drain"));
            assert!(!events.contains(&"drop_assets"));
            if operation == "destroy_device" {
                assert!(!events.contains(&"destroy_tap"));
            }
        }
    }
    fn partial_assets(fail: Vec<&'static str>) -> (Assets, Arc<Native>, Weak<()>) {
        let (mut assets, _, callback) = assets(None);
        let native = Arc::new(Native {
            fail,
            ..Default::default()
        });
        assets.native = native.clone();
        assets.device = false;
        assets.queue = false;
        (assets, native, callback)
    }
    fn prepare_system(assets: &mut Assets) -> Result<(), TapError> {
        for operation in ["uid", "format", "aggregate", "callback"] {
            assets.native.call(operation).map_err(TapError::platform)?;
            if operation == "aggregate" {
                assets.device = true;
            }
            if operation == "callback" {
                assets.queue = true;
            }
        }
        Ok(())
    }
    #[test]
    fn partial_setup_failure_reports_failed_tap_release_instead_of_discarding_it() {
        for phase in ["uid", "format", "aggregate"] {
            let (assets, native, callback) = partial_assets(vec![phase, "destroy_tap"]);
            let error = CaptureOwner::prepare(assets, prepare_system)
                .err()
                .expect("setup failed");
            assert_eq!(
                error.cleanup_state(),
                Some(CleanupState::Stopped),
                "{phase}: {error}"
            );
            let message = error.to_string();
            assert!(message.contains(phase) && message.contains("destroy_tap"));
            assert_eq!(*native.retained.lock().unwrap(), [CleanupState::Stopped]);
            assert!(callback.upgrade().is_some(), "retain the incomplete bundle");
            assert!(!native.events.lock().unwrap().contains(&"drop_assets"));
        }
    }
    #[test]
    fn partial_setup_failure_reports_failed_aggregate_release_before_releasing_its_tap() {
        let (assets, native, callback) = partial_assets(vec!["callback", "destroy_device"]);
        let error = CaptureOwner::prepare(assets, prepare_system)
            .err()
            .expect("setup failed");
        assert_eq!(error.cleanup_state(), Some(CleanupState::Stopped));
        assert!(
            error.to_string().contains("callback") && error.to_string().contains("destroy_device")
        );
        assert_eq!(
            *native.events.lock().unwrap(),
            ["uid", "format", "aggregate", "callback", "destroy_device"]
        );
        assert_eq!(*native.retained.lock().unwrap(), [CleanupState::Stopped]);
        assert!(callback.upgrade().is_some());
    }
    #[test]
    fn setup_errors_release_only_acquired_dependencies_and_preserve_the_setup_error() {
        for phase in ["uid", "format", "aggregate", "callback"] {
            let (assets, native, callback) = partial_assets(vec![phase]);
            let error = CaptureOwner::prepare(assets, prepare_system)
                .err()
                .expect("setup failed");
            assert!(matches!(error, TapError::Platform(_)), "{error}");
            assert!(error.to_string().contains(phase));
            let events = native.events.lock().unwrap();
            let end = if phase == "callback" {
                vec!["destroy_device", "destroy_tap", "drop_assets"]
            } else {
                vec!["destroy_tap", "drop_assets"]
            };
            assert!(events.ends_with(&end), "{phase}: {events:?}");
            for forbidden in ["register", "start", "stop", "destroy_proc", "drain"] {
                assert!(!events.contains(&forbidden), "{phase}: {events:?}");
            }
            assert!(callback.upgrade().is_none());
            assert!(native.retained.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn successful_preparation_does_not_register_or_start_until_requested() {
        let (assets, native, callback) = partial_assets(vec![]);
        let (owner, ()) = CaptureOwner::prepare(assets, prepare_system).unwrap();
        assert_eq!(
            *native.events.lock().unwrap(),
            ["uid", "format", "aggregate", "callback"]
        );
        let mut owner = owner.start_prepared().unwrap();
        owner.close().unwrap();
        assert_eq!(
            *native.events.lock().unwrap(),
            [
                "uid",
                "format",
                "aggregate",
                "callback",
                "register",
                "start",
                "stop",
                "destroy_proc",
                "drain",
                "destroy_device",
                "destroy_tap",
                "drop_assets"
            ]
        );
        assert!(callback.upgrade().is_none());
    }
    #[test]
    fn dropping_prepared_capture_releases_dependencies_without_starting() {
        let (assets, native, callback) = partial_assets(vec![]);
        let (owner, ()) = CaptureOwner::prepare(assets, prepare_system).unwrap();
        drop(owner);
        assert_eq!(
            *native.events.lock().unwrap(),
            [
                "uid",
                "format",
                "aggregate",
                "callback",
                "drain",
                "destroy_device",
                "destroy_tap",
                "drop_assets"
            ]
        );
        assert!(callback.upgrade().is_none());
    }
}
