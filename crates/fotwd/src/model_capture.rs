//! Carry model-setup exclusion through the real platform/tap owner lifetimes.
use crate::model_setup::WorkLease;
use fotw_audio::*;
use std::sync::Arc;

pub(crate) fn lease_tap(tap: Box<dyn AudioTap>, lease: Arc<WorkLease>) -> Box<dyn AudioTap> {
    Box::new(LeasedTap {
        tap: Some(tap),
        lease,
        started: false,
        stop_attempted: false,
        stopped: false,
    })
}
struct LeasedTap {
    tap: Option<Box<dyn AudioTap>>,
    lease: Arc<WorkLease>,
    started: bool,
    stop_attempted: bool,
    stopped: bool,
}
impl LeasedTap {
    fn observe(&mut self, result: &Result<(), TapError>) {
        self.stopped |= result.is_ok()
            || result
                .as_ref()
                .err()
                .and_then(TapError::cleanup_state)
                .is_some_and(|s| s.capture_stopped());
    }
}
impl AudioTap for LeasedTap {
    fn id(&self) -> &TapId {
        self.tap.as_ref().unwrap().id()
    }
    fn format(&self) -> StreamFormat {
        self.tap.as_ref().unwrap().format()
    }
    fn format_is_authoritative(&self) -> bool {
        self.tap.as_ref().unwrap().format_is_authoritative()
    }
    fn start(&mut self, sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        self.started = true;
        self.stopped = false;
        self.stop_attempted = false;
        let result = self.tap.as_mut().unwrap().start(sink);
        if let Err(error) = &result {
            self.stopped = error.cleanup_state().is_some_and(|s| s.capture_stopped());
        }
        result
    }
    fn stop(&mut self) -> Result<(), TapError> {
        self.stop_attempted = true;
        let result = self.tap.as_mut().unwrap().stop();
        self.observe(&result);
        result
    }
    fn stop_observed(&mut self, stopped: &mut dyn FnMut()) -> Result<(), TapError> {
        self.stop_attempted = true;
        let mut positive = false;
        let result = self.tap.as_mut().unwrap().stop_observed(&mut || {
            positive = true;
            stopped();
        });
        self.stopped |= positive;
        self.observe(&result);
        result
    }
}
struct UnconfirmedCapture {
    lease: Arc<WorkLease>,
    possible: bool,
}
impl Drop for UnconfirmedCapture {
    fn drop(&mut self) {
        if self.possible {
            WorkLease::quarantine(self.lease.clone());
        }
    }
}
impl Drop for LeasedTap {
    fn drop(&mut self) {
        // Keep the fallback guard alive across both Stop and native destruction,
        // including unwinding. A panic cannot serve as positive Stop evidence.
        let mut capture = UnconfirmedCapture {
            lease: self.lease.clone(),
            possible: self.started && !self.stopped,
        };
        if self.started && !self.stop_attempted {
            let _ = self.stop();
        }
        capture.possible = self.started && !self.stopped;
        drop(self.tap.take());
    }
}

struct LeasedWatch {
    _watch: Box<dyn DeviceWatch>,
    _lease: Arc<WorkLease>,
}
impl DeviceWatch for LeasedWatch {}
struct LeasedPlatform {
    inner: Arc<dyn AudioPlatform>,
    lease: Arc<WorkLease>,
}
pub(crate) fn lease_platform(
    inner: Arc<dyn AudioPlatform>,
    lease: Arc<WorkLease>,
) -> Arc<dyn AudioPlatform> {
    Arc::new(LeasedPlatform { inner, lease })
}
impl AudioPlatform for LeasedPlatform {
    fn caps(&self) -> PlatformCaps {
        self.inner.caps()
    }
    fn permission(&self, p: Permission) -> PermissionState {
        self.inner.permission(p)
    }
    fn request_permission(&self, p: Permission) -> BoxFuture<'static, PermissionState> {
        self.inner.request_permission(p)
    }
    fn mics(&self) -> Vec<DeviceInfo> {
        self.inner.mics()
    }
    fn capturable_apps(&self) -> Vec<AppInfo> {
        self.inner.capturable_apps()
    }
    fn open_mic(&self, id: &DeviceId, hint: FormatRequest) -> Result<Box<dyn AudioTap>, TapError> {
        self.inner
            .open_mic(id, hint)
            .map(|tap| lease_tap(tap, self.lease.clone()))
    }
    fn open_system(
        &self,
        scope: SystemScope,
        hint: FormatRequest,
    ) -> Result<Box<dyn AudioTap>, TapError> {
        self.inner
            .open_system(scope, hint)
            .map(|tap| lease_tap(tap, self.lease.clone()))
    }
    fn events(&self) -> std::sync::mpsc::Receiver<PlatformEvent> {
        self.inner.events()
    }
    fn watch_devices(
        &self,
        signal: Arc<DeviceChangeSignal>,
    ) -> Result<Box<dyn DeviceWatch>, TapError> {
        self.inner.watch_devices(signal).map(|watch| {
            Box::new(LeasedWatch {
                _watch: watch,
                _lease: self.lease.clone(),
            }) as Box<dyn DeviceWatch>
        })
    }
    fn output_activity(&self) -> OutputActivity {
        self.inner.output_activity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};
    struct Sink;
    impl FrameSink for Sink {
        fn on_frames(&mut self, _: &[f32], _: CaptureTimestamp, _: FrameFlags) {}
        fn on_error(&mut self, _: TapError) {}
    }
    struct Tap {
        id: TapId,
        unknown: bool,
        drop_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    }
    impl AudioTap for Tap {
        fn id(&self) -> &TapId {
            &self.id
        }
        fn format(&self) -> StreamFormat {
            StreamFormat::new(16000, 1, SampleFormat::F32)
        }
        fn format_is_authoritative(&self) -> bool {
            true
        }
        fn start(&mut self, _: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
            Ok(self.format())
        }
        fn stop(&mut self) -> Result<(), TapError> {
            if self.unknown {
                Err(TapError::Cleanup {
                    state: CleanupState::Unconfirmed,
                    message: "injected fixture".into(),
                })
            } else {
                Ok(())
            }
        }
    }
    impl Drop for Tap {
        fn drop(&mut self) {
            if let Some((began, release)) = self.drop_gate.take() {
                began.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(3)).unwrap();
            }
        }
    }
    #[test]
    fn positive_stop_does_not_release_exclusion_before_native_destructor_returns() {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(WorkLease::capture(&root.path().join("sessions")).unwrap());
        let (began, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let mut tap = lease_tap(
            Box::new(Tap {
                id: TapId::system_default(),
                unknown: false,
                drop_gate: Some((began, gate)),
            }),
            lease.clone(),
        );
        drop(lease);
        tap.start(Box::new(Sink)).unwrap();
        let worker = std::thread::spawn(move || {
            tap.stop().unwrap();
            drop(tap);
        });
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let file = OpenOptionsForTest::exclusive(root.path());
        let blocked = file.is_err();
        drop(file);
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(blocked);
        assert!(OpenOptionsForTest::exclusive(root.path()).is_ok());
    }
    struct OpenOptionsForTest;
    impl OpenOptionsForTest {
        fn exclusive(root: &std::path::Path) -> std::io::Result<std::fs::File> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(root.join("model-work.lock"))?;
            file.try_lock().map_err(std::io::Error::from)?;
            Ok(file)
        }
    }
    #[test]
    fn unconfirmed_stop_cannot_admit_model_setup_after_the_tap_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(WorkLease::capture(&root.path().join("sessions")).unwrap());
        let mut tap = lease_tap(
            Box::new(Tap {
                id: TapId::system_default(),
                unknown: true,
                drop_gate: None,
            }),
            lease.clone(),
        );
        drop(lease);
        tap.start(Box::new(Sink)).unwrap();
        assert!(tap.stop().is_err());
        drop(tap);
        assert!(OpenOptionsForTest::exclusive(root.path()).is_err());
    }
    #[test]
    fn repeated_unconfirmed_cleanup_retains_one_guard_for_the_same_library() {
        let root = tempfile::tempdir().unwrap();
        let owner = Arc::new(WorkLease::capture(&root.path().join("sessions")).unwrap());
        for _ in 0..32 {
            let mut tap = lease_tap(
                Box::new(Tap {
                    id: TapId::system_default(),
                    unknown: true,
                    drop_gate: None,
                }),
                owner.clone(),
            );
            tap.start(Box::new(Sink)).unwrap();
            assert!(tap.stop().is_err());
            drop(tap);
        }
        assert_eq!(
            Arc::strong_count(&owner),
            2,
            "quarantine multiplied retained owners per failed Stop"
        );
        assert!(OpenOptionsForTest::exclusive(root.path()).is_err());
    }
    #[test]
    fn native_stop_or_destructor_panics_do_not_release_unknown_capture_admission() {
        struct PanickingTap {
            id: TapId,
            stop_panic: bool,
        }
        impl AudioTap for PanickingTap {
            fn id(&self) -> &TapId {
                &self.id
            }
            fn format(&self) -> StreamFormat {
                StreamFormat::new(16000, 1, SampleFormat::F32)
            }
            fn format_is_authoritative(&self) -> bool {
                true
            }
            fn start(&mut self, _: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
                Ok(self.format())
            }
            fn stop(&mut self) -> Result<(), TapError> {
                if self.stop_panic {
                    panic!("injected Stop panic")
                };
                Err(TapError::Cleanup {
                    state: CleanupState::Unconfirmed,
                    message: "fixture".into(),
                })
            }
        }
        impl Drop for PanickingTap {
            fn drop(&mut self) {
                if !self.stop_panic {
                    panic!("injected destructor panic")
                }
            }
        }
        for stop_panic in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let lease = Arc::new(WorkLease::capture(&root.path().join("sessions")).unwrap());
            let mut tap = lease_tap(
                Box::new(PanickingTap {
                    id: TapId::system_default(),
                    stop_panic,
                }),
                lease,
            );
            tap.start(Box::new(Sink)).unwrap();
            if !stop_panic {
                assert!(tap.stop().is_err());
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(tap)));
            assert!(result.is_err());
            assert!(
                OpenOptionsForTest::exclusive(root.path()).is_err(),
                "native unwind discarded unknown capture ownership"
            );
        }
    }
}
