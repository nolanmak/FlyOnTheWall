//! Document routes obey the same ingress boundary as transcripts.
mod common;
use fotw_web::documents::{DocumentControl, DocumentRequest, DocumentState};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Fake(AtomicUsize);
impl DocumentControl for Fake {
    fn run(
        &self,
        _: String,
        _: DocumentRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DocumentState, String>> + Send + '_>,
    > {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(DocumentState {
                document: None,
                revision: 0,
                automatic: true,
            })
        })
    }
}
#[tokio::test]
async fn authentication_and_validation_precede_generation() {
    let h = common::start().await;
    let control = Arc::new(Fake(AtomicUsize::new(0)));
    h.state.set_documents(control.clone());
    let path = format!("/api/meetings/{}/document", common::MEETING_ID);
    let res = h
        .post(&path, &h.anonymous(), Some(r#"{"action":"generate"}"#))
        .await;
    assert_eq!(res.status, 404);
    assert!(res.body.is_empty());
    let too_long = serde_json::json!({"action":"generate","purpose":"x".repeat(2001)}).to_string();
    let res = h.post(&path, &h.authorised(), Some(&too_long)).await;
    assert!(res.body.contains("invalid or too long"));
    assert_eq!(control.0.load(Ordering::SeqCst), 0);
    let res = h
        .post(&path, &h.authorised(), Some(r#"{"action":"load"}"#))
        .await;
    assert!(res.body.contains("automatic"));
    assert_eq!(control.0.load(Ordering::SeqCst), 1);
}
