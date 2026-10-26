//! A selected but unenforceable policy must fail before capture or speech setup.
use fotw_web::RecorderControl;
use fotwd::{
    recording::{DaemonRecorder, ReadyTap},
    session::SegmentTap,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[tokio::test(flavor = "multi_thread")]
async fn invalid_capture_policy_cannot_open_devices_or_configure_any_stt_backend() {
    let root = tempfile::tempdir().unwrap();
    let opens = Arc::new(AtomicUsize::new(0));
    let opened = opens.clone();
    let configs = Arc::new(AtomicUsize::new(0));
    let configured = configs.clone();
    let recorder = DaemonRecorder::with_parts(
        root.path().join("sessions"),
        tokio::runtime::Handle::current(),
        SegmentTap::default(),
        ReadyTap::default(),
        Box::new(move || {
            opened.fetch_add(1, Ordering::SeqCst);
            Err("device opener must not run".into())
        }),
        Box::new(move || {
            configured.fetch_add(1, Ordering::SeqCst);
            fotwd::session::Transcription::Disabled
        }),
        Box::new(|_, _| panic!("no capture to persist")),
        Duration::from_secs(5),
        Duration::from_secs(2),
    )
    .with_capture_scope(|| Err("selected Brave helper identity cannot be confirmed".into()));
    let error = recorder.start().unwrap_err().to_string();
    assert!(error.contains("helper identity"), "{error}");
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    assert_eq!(configs.load(Ordering::SeqCst), 0);
    assert_eq!(recorder.status().state, fotw_web::RecordingState::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_during_application_resolution_cannot_configure_speech_or_open_capture() {
    use std::sync::{Condvar, Mutex, atomic::AtomicBool};
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
    let resolving = Arc::new(AtomicBool::new(false));
    let entered = resolving.clone();
    let blocked = release.0.clone();
    let opens = Arc::new(AtomicUsize::new(0));
    let opened = opens.clone();
    let configs = Arc::new(AtomicUsize::new(0));
    let configured = configs.clone();
    let recorder = Arc::new(
        DaemonRecorder::with_parts(
            root.path().join("sessions"),
            tokio::runtime::Handle::current(),
            SegmentTap::default(),
            ReadyTap::default(),
            Box::new(move || {
                opened.fetch_add(1, Ordering::SeqCst);
                Err("capture must not open".into())
            }),
            Box::new(move || {
                configured.fetch_add(1, Ordering::SeqCst);
                fotwd::session::Transcription::Disabled
            }),
            Box::new(|_, _| panic!("no capture to persist")),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .with_capture_scope(move || {
            entered.store(true, Ordering::Release);
            let mut done = blocked.0.lock().unwrap();
            while !*done {
                done = blocked.1.wait(done).unwrap();
            }
            Ok(fotw_audio::SystemScope::DefaultOutputMix)
        }),
    );
    let cancel = fotwd::session::StopSignal::new();
    let stopped = cancel.clone();
    let worker = recorder.clone();
    let start = tokio::task::spawn_blocking(move || {
        worker.start_native(fotw_shell::StartOrigin::Menu, true, stopped)
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !resolving.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    cancel.stop();
    drop(release);
    assert!(start.await.unwrap().is_err());
    assert_eq!(
        configs.load(Ordering::SeqCst),
        0,
        "cancelled resolution must not read speech/provider configuration"
    );
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    assert_eq!(recorder.status().state, fotw_web::RecordingState::Idle);
    assert_eq!(recorder.shutdown_status().outstanding, 0);
}
