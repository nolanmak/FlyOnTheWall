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
