//! Provisioning actions are explicit and pass existing ingress policy.
mod common;
use fotw_web::model_setup::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Debug, Default)]
struct Fake(AtomicUsize);
impl ModelSetupControl for Fake {
    fn read(&self) -> Status {
        Status {
            models: vec![],
            job: Job::default(),
        }
    }
    fn start(&self, _: Request) -> Result<Status, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(self.read())
    }
    fn cancel(&self, _: &str) -> Result<Status, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(self.read())
    }
}
#[tokio::test]
async fn provisioning_only_accepts_authenticated_explicit_typed_actions() {
    let h = common::start().await;
    let c = Arc::new(Fake::default());
    h.state.set_model_setup(c.clone());
    assert_eq!(h.get("/api/models", &h.authorised()).await.status, 200);
    for route in ["/api/models/install", "/api/models/cancel"] {
        assert_eq!(
            h.post(
                route,
                &h.anonymous(),
                Some(r#"{"source":"download","model":"small.en"}"#)
            )
            .await
            .status,
            404
        );
    }
    for body in [
        r#"{}"#,
        r#"{"source":"unknown","model":"small.en"}"#,
        r#"{"source":"download","model":"small.en","path":"/x"}"#,
    ] {
        assert_eq!(
            h.post("/api/models/install", &h.authorised(), Some(body))
                .await
                .status,
            404
        );
    }
    assert_eq!(c.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.post(
            "/api/models/install",
            &h.authorised(),
            Some(r#"{"source":"download","model":"small.en"}"#)
        )
        .await
        .status,
        200
    );
    assert_eq!(
        h.post(
            "/api/models/cancel",
            &h.authorised(),
            Some(r#"{"job_id":"fixture-job"}"#)
        )
        .await
        .status,
        200
    );
    assert_eq!(c.0.load(Ordering::SeqCst), 2);
}
