//! Recovery controls stay behind the normal ingress guard.
mod common;
use fotw_web::speech_recovery::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Debug, Default)]
struct Fake {
    reads: AtomicUsize,
    starts: AtomicUsize,
    cancels: AtomicUsize,
}
fn status() -> RecoveryStatus {
    RecoveryStatus {
        phase: RecoveryPhase::Incomplete,
        sources: vec![],
        worker_running: false,
        can_retry: true,
        message: None,
    }
}
impl SpeechRecoveryControl for Fake {
    fn read(&self, _: &str) -> Result<RecoveryStatus, RecoveryError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(status())
    }
    fn retry(&self, _: &str) -> Result<RecoveryStatus, RecoveryError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(RecoveryStatus {
            phase: RecoveryPhase::Preparing,
            worker_running: true,
            can_retry: false,
            ..status()
        })
    }
    fn cancel(&self, _: &str) -> Result<RecoveryStatus, RecoveryError> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(RecoveryStatus {
            phase: RecoveryPhase::Stopping,
            worker_running: true,
            can_retry: false,
            ..status()
        })
    }
}
#[tokio::test]
async fn authenticated_retry_is_a_control_action_and_get_never_starts_inference() {
    let h = common::start().await;
    let c = Arc::new(Fake::default());
    h.state.set_speech_recovery(c.clone());
    let read = h
        .get("/api/meetings/owned/speech-recovery", &h.authorised())
        .await;
    assert_eq!(read.status, 200);
    assert!(read.header("content-security-policy").is_some());
    assert_eq!(c.starts.load(Ordering::SeqCst), 0);
    let retry = h
        .post(
            "/api/meetings/owned/speech-recovery/retry",
            &h.authorised(),
            Some("{}"),
        )
        .await;
    assert_eq!(retry.status, 200);
    assert!(retry.body.contains("preparing"));
    assert_eq!(c.starts.load(Ordering::SeqCst), 1);
    let cancel = h
        .post(
            "/api/meetings/owned/speech-recovery/cancel",
            &h.authorised(),
            Some("{}"),
        )
        .await;
    assert_eq!(cancel.status, 200);
    assert!(cancel.body.contains("stopping"));
    assert_eq!(c.cancels.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn anonymous_and_malformed_actions_cannot_reach_recovery_or_choose_a_destination() {
    let h = common::start().await;
    let c = Arc::new(Fake::default());
    h.state.set_speech_recovery(c.clone());
    assert_eq!(
        h.get("/api/meetings/owned/speech-recovery", &h.anonymous())
            .await
            .status,
        404
    );
    assert_eq!(
        h.post(
            "/api/meetings/owned/speech-recovery/retry",
            &h.anonymous(),
            Some("{}")
        )
        .await
        .status,
        404
    );
    for body in [
        r#"{"endpoint":"https://unexpected.invalid"}"#,
        "null",
        "broken",
    ] {
        let r = h
            .post(
                "/api/meetings/owned/speech-recovery/retry",
                &h.authorised(),
                Some(body),
            )
            .await;
        assert_eq!(r.status, 404);
        assert!(r.body.is_empty());
    }
    assert_eq!(c.reads.load(Ordering::SeqCst), 0);
    assert_eq!(c.starts.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn missing_recovery_control_is_indistinguishable_from_missing_route() {
    let h = common::start().await;
    let absent = h
        .get("/api/meetings/owned/speech-recovery", &h.authorised())
        .await;
    let missing = h.get("/api/missing", &h.authorised()).await;
    assert_eq!(absent.bytes_without_date(), missing.bytes_without_date());
}
