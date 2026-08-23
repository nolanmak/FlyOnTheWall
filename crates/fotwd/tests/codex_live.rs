//! The one test that drives the real `codex` binary end to end.
//!
//! Self-skipping, like `github_live` and the real-keychain tests: CI has no
//! Codex login, and a test that spends someone's subscription must never run
//! by accident. Opt in when you have `codex` authed:
//!
//! ```sh
//! FOTW_CODEX_LIVE=1 cargo test -p fotwd --test codex_live -- --nocapture
//! ```
//!
//! It runs a tiny transcript through [`CodexCliAdapter`] over the production
//! [`TokioCliRunner`] — the exact argv, stdin, subprocess and JSONL parse the
//! daemon uses — and asserts a non-empty answer comes back. That is the seam
//! the unit tests fake; this is the one place the fake meets the binary.

use std::sync::Arc;
use std::time::Duration;

use fotw_summarize::adapter::{DocumentPayload, LlmAdapter, LlmRequest};
use fotw_summarize::capabilities::CacheTtl;
use fotw_summarize::codex_cli::CodexCliAdapter;
use fotw_summarize::document::TranscriptDocument;
use fotw_summarize::testing::sample_meeting;
use fotwd::engine::TokioCliRunner;

#[tokio::test]
async fn a_real_codex_run_summarises_a_tiny_transcript() {
    if std::env::var("FOTW_CODEX_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set FOTW_CODEX_LIVE=1 (with codex authed) to run");
        return;
    }
    let binary = std::env::var("FOTW_CODEX_BIN")
        .unwrap_or_else(|_| "/Applications/Codex.app/Contents/Resources/codex".to_owned());

    let document = TranscriptDocument::from_segments(&sample_meeting());
    let indices = document.all_indices();
    let request = LlmRequest {
        model: String::new(),
        system: "You summarise meeting transcripts. Be terse.".to_owned(),
        document: Some(DocumentPayload {
            document,
            indices,
            cache_ttl: CacheTtl::None,
            title: "Storage migration sync".to_owned(),
        }),
        user_notes: None,
        instruction: "In one sentence, what did they decide?".to_owned(),
        citations: false,
        output_format: None,
        effort: None,
        max_output_tokens: 512,
    };

    let runner = Arc::new(TokioCliRunner::new(binary.into(), Duration::from_secs(180)));
    let adapter = CodexCliAdapter::new(runner, None);

    let response = adapter
        .complete(&request)
        .await
        .expect("the real codex run should return an answer");
    let text = response.text();
    eprintln!("codex said: {text}");
    assert!(!text.trim().is_empty(), "codex returned an empty answer");
}
