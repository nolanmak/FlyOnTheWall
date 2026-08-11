//! Fakes for testing the pipeline with no API key and no socket.
//!
//! Shipped in the crate proper rather than behind `#[cfg(test)]` so the daemon
//! and the integration tests above this crate can use them too, matching
//! `fotw_audio::testing` (spec 5.6).
//!
//! The hard constraint this module exists to satisfy: **CI has no secrets.**
//! Every assertion about the two-call pipeline — including "these two fields
//! are never in the same request body" — has to be made against a recorded
//! request, not a live one. [`MockTransport`] is that recorder.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use fotw_stt::transcript::{Source, TimestampSource, TranscriptSegment};

use crate::error::SummarizeError;
use crate::transport::{HttpRequest, HttpResponse, HttpTransport};

/// How many times [`block_on`] polls before declaring the future stuck.
pub const BLOCK_ON_POLL_LIMIT: usize = 10_000;

/// Drive a future to completion on the current thread.
///
/// Every future in this crate's test surface is either immediately ready or
/// resolved by a mock, so this needs no reactor — which is what keeps an async
/// runtime out of the dependency graph entirely. Same trick, same reasoning as
/// `fotw_audio::testing::block_on`.
///
/// # Panics
///
/// Panics after [`BLOCK_ON_POLL_LIMIT`] polls. The waker is a no-op, so a
/// future that genuinely needs waking would spin forever; a bounded panic with
/// a message beats a wedged CI runner with none.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    for _ in 0..BLOCK_ON_POLL_LIMIT {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::hint::spin_loop();
    }
    panic!("future still pending after {BLOCK_ON_POLL_LIMIT} polls; it needs a real runtime");
}

/// A [`TranscriptSegment`] with everything irrelevant to summarization filled
/// in with defaults.
///
/// `id` is taken as a parameter rather than minted so tests can assert on
/// ordering and revision collapse deterministically.
#[must_use]
pub fn segment(
    id: &str,
    speaker: &str,
    start_ms: u64,
    end_ms: u64,
    text: &str,
) -> TranscriptSegment {
    TranscriptSegment {
        id: id.to_string(),
        session_id: "test-session".to_string(),
        source: Source::System,
        speaker: if speaker.is_empty() {
            None
        } else {
            Some(speaker.to_string())
        },
        text: text.to_string(),
        start_ms,
        end_ms,
        words: Vec::new(),
        confidence: Some(0.95),
        language: Some("en".to_string()),
        is_final: true,
        revision: 0,
        provider: "mock".to_string(),
        model: "mock-1".to_string(),
        timestamp_source: TimestampSource::Provider,
    }
}

/// A short, well-formed meeting: two speakers, one decision, one commitment
/// with an owner, one commitment with nobody assigned.
///
/// Shared by the validator and pipeline tests so that "what the transcript
/// actually says" is written down once.
#[must_use]
pub fn sample_meeting() -> Vec<TranscriptSegment> {
    vec![
        // "Hi, Alice here" rather than "Alice here": the validator's
        // proper-noun heuristic deliberately ignores sentence-initial
        // capitals, so a name in that position is a known false negative and
        // would make this fixture test the limitation instead of the rule.
        // `validate::tests::a_name_only_ever_spoken_sentence_initially_is_a_known_false_negative`
        // covers that case on purpose.
        segment(
            "s0",
            "S0",
            0,
            4_000,
            "Hi, Alice here. Let's start with the migration.",
        ),
        segment(
            "s1",
            "S1",
            4_000,
            11_000,
            "We agreed to move the storage layer to SQLite before the beta, and we \
             looked at Postgres but it needs a server the user does not have.",
        ),
        segment(
            "s2",
            "S0",
            11_000,
            17_000,
            "I will write the migration script by Friday.",
        ),
        segment(
            "s3",
            "S1",
            17_000,
            23_000,
            "Somebody needs to update the docs, we can figure out who later.",
        ),
        segment(
            "s4",
            "S0",
            23_000,
            29_000,
            "Open question is whether we keep the old export format.",
        ),
    ]
}

/// A recorded exchange: what the transport was asked for, and what it answered.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// The URL the adapter posted to.
    pub url: String,
    /// Header names and values, in the order the adapter set them.
    pub headers: Vec<(String, String)>,
    /// The request body, already parsed. Panics at record time if the adapter
    /// sent something that is not JSON, which is itself a useful failure.
    pub body: serde_json::Value,
}

/// An [`HttpTransport`] that records every request and replays queued
/// responses.
///
/// Responses are consumed in order; running out is an error rather than a
/// panic, so a test that makes an unexpected extra call fails with a message
/// about the call instead of a poisoned mutex.
#[derive(Debug, Clone, Default)]
pub struct MockTransport {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Debug, Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    responses: Vec<Result<HttpResponse, SummarizeError>>,
}

impl MockTransport {
    /// A transport with no queued responses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a 200 response with this JSON body.
    #[must_use]
    pub fn with_json(self, body: serde_json::Value) -> Self {
        self.push(Ok(HttpResponse {
            status: 200,
            body: body.to_string().into_bytes(),
        }));
        self
    }

    /// Queue a raw response, status included.
    #[must_use]
    pub fn with_response(self, response: HttpResponse) -> Self {
        self.push(Ok(response));
        self
    }

    /// Queue a transport-level failure.
    #[must_use]
    pub fn with_error(self, error: SummarizeError) -> Self {
        self.push(Err(error));
        self
    }

    fn push(&self, response: Result<HttpResponse, SummarizeError>) {
        self.lock().responses.push(response);
    }

    /// Every request made so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.lock().requests.clone()
    }

    /// The `n`th request, panicking with a useful message if it was never made.
    ///
    /// # Panics
    ///
    /// Panics when fewer than `n + 1` requests have been recorded.
    #[must_use]
    pub fn request(&self, n: usize) -> RecordedRequest {
        let requests = self.requests();
        assert!(
            n < requests.len(),
            "expected at least {} request(s), the pipeline made {}",
            n + 1,
            requests.len()
        );
        requests[n].clone()
    }

    /// How many requests have been made.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.lock().requests.len()
    }

    /// Lock the state, recovering from a poisoned mutex.
    ///
    /// A panicking test leaves the mutex poisoned; propagating that would
    /// replace the real assertion failure with a poison error in every
    /// subsequent line of the same test.
    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl HttpTransport for MockTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, SummarizeError>> + Send + 'a>> {
        let body = serde_json::from_slice(&request.body).unwrap_or_else(|error| {
            panic!("adapter sent a body that is not JSON ({error}); that is always a bug")
        });
        let mut state = self.lock();
        state.requests.push(RecordedRequest {
            url: request.url,
            headers: request.headers,
            body,
        });
        let index = state.requests.len() - 1;
        let response = if index < state.responses.len() {
            match &state.responses[index] {
                Ok(response) => Ok(response.clone()),
                Err(error) => Err(SummarizeError::Transport(error.to_string())),
            }
        } else {
            Err(SummarizeError::Transport(format!(
                "MockTransport had no queued response for request #{index}"
            )))
        };
        Box::pin(std::future::ready(response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_resolves_a_ready_future() {
        assert_eq!(block_on(std::future::ready(7)), 7);
    }

    #[test]
    fn mock_records_the_body_it_was_sent() {
        let transport = MockTransport::new().with_json(serde_json::json!({"ok": true}));
        let response = block_on(transport.post(HttpRequest {
            url: "https://example.invalid/v1/messages".to_string(),
            headers: vec![("x-test".to_string(), "1".to_string())],
            body: serde_json::json!({"model": "m"}).to_string().into_bytes(),
        }))
        .expect("queued response");

        assert_eq!(response.status, 200);
        assert_eq!(transport.call_count(), 1);
        assert_eq!(transport.request(0).body["model"], serde_json::json!("m"));
    }

    #[test]
    fn an_unexpected_extra_call_errors_rather_than_panics() {
        let transport = MockTransport::new();
        let result = block_on(transport.post(HttpRequest {
            url: "https://example.invalid/".to_string(),
            headers: Vec::new(),
            body: b"{}".to_vec(),
        }));
        assert!(result.is_err());
    }
}
