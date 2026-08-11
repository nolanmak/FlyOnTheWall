//! The Anthropic Messages adapter (spec 8.2, 8.4).
//!
//! Builds the request body, hands it to an injected [`HttpTransport`], and
//! normalizes the response into [`LlmResponse`]. No socket and no TLS live
//! here — see [`crate::transport`] for why that is the point rather than a gap.
//!
//! **Block order is `system → document → user notes → instruction`** (spec
//! 8.4), and it is a cost decision, not an aesthetic one. Prompt caching works
//! on prefixes: everything before the cache breakpoint is reusable only if it
//! is byte-identical to last time. System prompt and transcript are stable
//! across both calls of the pipeline; notes and instruction are not. Putting
//! the instruction before the document would make the cached prefix empty and
//! double the input bill.
//!
//! **What this module deliberately never sends** (spec 8.2): `temperature`,
//! `top_p`, `top_k` and `budget_tokens` all return 400 on Opus 5, and
//! `thinking: {type: "disabled"}` is capped at effort `high` and introduces two
//! documented failure modes — tool calls emitted as plain text that silently
//! never run, and `<thinking>` tags leaking into visible output. There is no
//! field on [`LlmRequest`] that can express any of them, and
//! [`tests::the_body_never_contains_a_parameter_that_400s_on_opus_5`] asserts
//! they are absent from the bytes.

use serde_json::{Value, json};

use crate::adapter::{
    AnnotatedBlock, Citation, DocumentPayload, LlmAdapter, LlmRequest, LlmResponse, Usage,
};
use crate::capabilities::Capabilities;
use crate::error::SummarizeError;
use crate::transport::{BoxFuture, HttpRequest, HttpTransport};

/// Default Messages endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// The API version header value this adapter is written against.
pub const API_VERSION: &str = "2023-06-01";

/// An adapter over the Anthropic Messages API.
pub struct AnthropicAdapter<T: HttpTransport> {
    transport: T,
    model: String,
    capabilities: Capabilities,
    endpoint: String,
    api_key: String,
}

impl<T: HttpTransport> AnthropicAdapter<T> {
    /// A new adapter against the default endpoint.
    pub fn new(transport: T, model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            transport,
            model: model.into(),
            capabilities: Capabilities::anthropic_frontier(),
            endpoint: DEFAULT_ENDPOINT.to_string(),
            api_key: api_key.into(),
        }
    }

    /// Point the adapter at another endpoint — a local mock, or a gateway.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Override the capability descriptor, e.g. for a smaller-context model.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Build the request body.
    ///
    /// Public so the pipeline's tests can assert on the bytes without a
    /// transport, and so a `--dry-run` cost preview (SUM-11) can price the
    /// exact payload that would be sent.
    ///
    /// # Errors
    ///
    /// Whatever [`LlmRequest::validate`] rejects.
    pub fn build_body(&self, request: &LlmRequest) -> Result<Value, SummarizeError> {
        request.validate(&self.capabilities)?;

        let mut content = Vec::new();
        if let Some(payload) = &request.document {
            content.push(document_block(payload, request.citations));
        }
        if let Some(notes) = &request.user_notes
            && !notes.trim().is_empty()
        {
            content.push(json!({
                "type": "text",
                "text": format!("The user's raw notes:\n\n{notes}"),
            }));
        }
        content.push(json!({ "type": "text", "text": request.instruction }));

        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_output_tokens,
            "system": [{ "type": "text", "text": request.system }],
            "messages": [{ "role": "user", "content": content }],
        });

        // `effort` and `format` share one object, which is the mechanical
        // reason spec 8.4's two features collide -- worth seeing in the code.
        let mut output_config = serde_json::Map::new();
        if let Some(effort) = request.effort {
            output_config.insert("effort".to_string(), json!(effort.wire_value()));
        }
        if let Some(schema) = &request.output_format {
            output_config.insert(
                "format".to_string(),
                json!({ "type": "json_schema", "schema": schema }),
            );
        }
        if !output_config.is_empty() {
            body["output_config"] = Value::Object(output_config);
        }

        Ok(body)
    }
}

/// The transcript as a custom-content document block (spec 8.4).
fn document_block(payload: &DocumentPayload, citations: bool) -> Value {
    let mut block = json!({
        "type": "document",
        "source": {
            "type": "content",
            "content": payload.document.blocks_for(&payload.indices),
        },
        "title": payload.title,
        "citations": { "enabled": citations },
    });

    // One breakpoint, on the last block of the stable prefix. A cache
    // breakpoint caches everything *before* it, so marking the document covers
    // the system prompt too and a second breakpoint would buy nothing.
    if let Some(ttl) = payload.cache_ttl.wire_value() {
        block["cache_control"] = json!({ "type": "ephemeral", "ttl": ttl });
    }
    block
}

impl<T: HttpTransport> LlmAdapter for AnthropicAdapter<T> {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn complete<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> BoxFuture<'a, Result<LlmResponse, SummarizeError>> {
        Box::pin(async move {
            let body = self.build_body(request)?;
            let http = HttpRequest {
                url: self.endpoint.clone(),
                headers: vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    ("anthropic-version".to_string(), API_VERSION.to_string()),
                    ("x-api-key".to_string(), self.api_key.clone()),
                ],
                body: serde_json::to_vec(&body)
                    .map_err(|error| SummarizeError::Decode(error.to_string()))?,
            };

            let response = self.transport.post(http).await?;
            if !(200..300).contains(&response.status) {
                return Err(SummarizeError::Http {
                    status: response.status,
                    body: response.body_text(),
                });
            }
            parse_response(&response.body)
        })
    }
}

/// Normalize a Messages response into [`LlmResponse`].
///
/// # Errors
///
/// [`SummarizeError::Decode`] if the payload is not the documented shape.
pub fn parse_response(body: &[u8]) -> Result<LlmResponse, SummarizeError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| SummarizeError::Decode(error.to_string()))?;

    let content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| SummarizeError::Decode("response has no `content` array".to_string()))?;

    let mut blocks = Vec::new();
    for item in content {
        // Thinking and tool_use blocks ride along in the same array. Skipping
        // by type rather than by position keeps this working when adaptive
        // thinking decides to emit one.
        if item.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let citations = item
            .get("citations")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(parse_citation).collect())
            .unwrap_or_default();
        blocks.push(AnnotatedBlock { text, citations });
    }

    let usage = value.get("usage").map_or_else(Usage::default, |usage| {
        let field = |name: &str| {
            usage
                .get(name)
                .and_then(Value::as_u64)
                .unwrap_or_default()
                .try_into()
                .unwrap_or(usize::MAX)
        };
        Usage {
            input_tokens: field("input_tokens"),
            output_tokens: field("output_tokens"),
            cache_creation_input_tokens: field("cache_creation_input_tokens"),
            cache_read_input_tokens: field("cache_read_input_tokens"),
        }
    });

    Ok(LlmResponse {
        blocks,
        stop_reason: value
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        usage,
    })
}

fn parse_citation(value: &Value) -> Option<Citation> {
    // Only content_block_location citations map onto segment indices. A
    // char_location citation would come from a plain-text document, which this
    // pipeline never sends, and silently treating its character offsets as
    // block indices would point every claim at the wrong timestamp.
    if value.get("type").and_then(Value::as_str) != Some("content_block_location") {
        return None;
    }
    let index = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    };
    Some(Citation {
        start_block_index: index("start_block_index")?,
        end_block_index: index("end_block_index")?,
        cited_text: value
            .get("cited_text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        document_index: index("document_index").unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{CacheTtl, Effort};
    use crate::document::TranscriptDocument;
    use crate::schema::EXTRACTION_SCHEMA;
    use crate::testing::{MockTransport, block_on, sample_meeting};

    fn document() -> TranscriptDocument {
        TranscriptDocument::from_segments(&sample_meeting())
    }

    fn payload(cache_ttl: CacheTtl) -> DocumentPayload {
        let document = document();
        DocumentPayload {
            indices: document.all_indices(),
            document,
            cache_ttl,
            title: "Meeting transcript".to_string(),
        }
    }

    fn call_a() -> LlmRequest {
        LlmRequest {
            model: "claude-opus-5".to_string(),
            system: "grounding contract".to_string(),
            document: Some(payload(CacheTtl::FiveMinutes)),
            user_notes: Some("- migration?".to_string()),
            instruction: "Write the notes.".to_string(),
            citations: true,
            output_format: None,
            effort: Some(Effort::Medium),
            max_output_tokens: 8_192,
        }
    }

    fn call_b() -> LlmRequest {
        LlmRequest {
            citations: false,
            output_format: Some(EXTRACTION_SCHEMA.clone()),
            user_notes: None,
            instruction: "Extract.".to_string(),
            ..call_a()
        }
    }

    fn adapter() -> AnthropicAdapter<MockTransport> {
        AnthropicAdapter::new(MockTransport::new(), "claude-opus-5", "test-key")
    }

    #[test]
    fn the_transcript_is_one_content_block_per_segment_inside_a_document() {
        let body = adapter().build_body(&call_a()).expect("valid request");
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], json!("document"));
        assert_eq!(block["source"]["type"], json!("content"));
        assert_eq!(
            block["source"]["content"].as_array().expect("blocks").len(),
            sample_meeting().len()
        );
        assert_eq!(block["citations"]["enabled"], json!(true));
    }

    #[test]
    fn block_order_is_document_then_notes_then_instruction() {
        // Spec 8.4's cache-prefix ordering. Reversing it would empty the
        // cacheable prefix and double the input bill.
        let body = adapter().build_body(&call_a()).expect("valid request");
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content array");
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], json!("document"));
        assert!(
            content[1]["text"]
                .as_str()
                .expect("notes")
                .contains("migration?")
        );
        assert_eq!(content[2]["text"], json!("Write the notes."));
    }

    #[test]
    fn empty_notes_do_not_produce_an_empty_block() {
        // SUM-01: notes are optional. An empty text block is a 400 on the API
        // and a wasted cache-invalidating byte if it is not.
        let request = LlmRequest {
            user_notes: Some("   \n ".to_string()),
            ..call_a()
        };
        let body = adapter().build_body(&request).expect("valid request");
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content array");
        assert_eq!(content.len(), 2);
    }

    #[test]
    fn the_cache_breakpoint_lands_on_the_document_with_the_requested_ttl() {
        let body = adapter().build_body(&call_a()).expect("valid request");
        let cache = &body["messages"][0]["content"][0]["cache_control"];
        assert_eq!(cache["type"], json!("ephemeral"));
        assert_eq!(cache["ttl"], json!("5m"));
    }

    #[test]
    fn no_cache_control_appears_when_caching_is_off() {
        let request = LlmRequest {
            document: Some(payload(CacheTtl::None)),
            ..call_a()
        };
        let body = adapter().build_body(&request).expect("valid request");
        assert!(body["messages"][0]["content"][0]["cache_control"].is_null());
    }

    #[test]
    fn call_a_sends_citations_and_no_output_format() {
        let body = adapter().build_body(&call_a()).expect("valid request");
        assert_eq!(
            body["messages"][0]["content"][0]["citations"]["enabled"],
            json!(true)
        );
        assert!(
            body["output_config"]["format"].is_null(),
            "Call A must not carry a format"
        );
        assert_eq!(body["output_config"]["effort"], json!("medium"));
    }

    #[test]
    fn call_b_sends_an_output_format_and_citations_off() {
        let body = adapter().build_body(&call_b()).expect("valid request");
        assert_eq!(
            body["messages"][0]["content"][0]["citations"]["enabled"],
            json!(false)
        );
        assert_eq!(
            body["output_config"]["format"]["type"],
            json!("json_schema")
        );
        assert_eq!(
            body["output_config"]["format"]["schema"],
            *EXTRACTION_SCHEMA
        );
    }

    #[test]
    fn a_request_carrying_both_never_reaches_the_transport() {
        // Spec 8.4: this is an HTTP 400. The adapter must not spend the round
        // trip discovering that.
        let transport = MockTransport::new();
        let adapter = AnthropicAdapter::new(transport.clone(), "claude-opus-5", "k");
        let both = LlmRequest {
            citations: true,
            output_format: Some(EXTRACTION_SCHEMA.clone()),
            ..call_a()
        };

        let error = block_on(adapter.complete(&both)).expect_err("must be refused");
        assert!(matches!(
            error,
            SummarizeError::CitationsWithStructuredOutput
        ));
        assert_eq!(
            transport.call_count(),
            0,
            "the adapter sent the forbidden combination anyway"
        );
    }

    #[test]
    fn the_body_never_contains_a_parameter_that_400s_on_opus_5() {
        // Spec 8.2: temperature, top_p, top_k and budget_tokens all 400, and
        // thinking must never be disabled. No LlmRequest field can express any
        // of them; this asserts it on the bytes so that a future "just add a
        // temperature knob" change fails here.
        for request in [call_a(), call_b()] {
            let text = adapter()
                .build_body(&request)
                .expect("valid request")
                .to_string();
            for banned in ["temperature", "top_p", "top_k", "budget_tokens"] {
                assert!(!text.contains(banned), "body contains `{banned}`");
            }
            assert!(!text.contains("\"thinking\""), "body mentions thinking");
            assert!(!text.contains("disabled"));
        }
    }

    #[test]
    fn effort_is_omitted_entirely_rather_than_sent_as_null() {
        let request = LlmRequest {
            effort: None,
            output_format: None,
            citations: false,
            ..call_a()
        };
        let body = adapter().build_body(&request).expect("valid request");
        assert!(body["output_config"].is_null());
    }

    #[test]
    fn the_auth_and_version_headers_are_set_on_the_wire() {
        let transport = MockTransport::new().with_json(json!({
            "content": [], "stop_reason": "end_turn", "usage": {}
        }));
        let adapter = AnthropicAdapter::new(transport.clone(), "claude-opus-5", "sk-test");
        block_on(adapter.complete(&call_a())).expect("mock response");

        let recorded = transport.request(0);
        assert_eq!(recorded.url, DEFAULT_ENDPOINT);
        assert!(
            recorded
                .headers
                .contains(&("x-api-key".to_string(), "sk-test".to_string()))
        );
        assert!(
            recorded
                .headers
                .contains(&("anthropic-version".to_string(), API_VERSION.to_string()))
        );
    }

    #[test]
    fn a_non_2xx_status_becomes_an_http_error_carrying_the_body() {
        let transport = MockTransport::new().with_response(crate::transport::HttpResponse {
            status: 429,
            body: b"{\"error\":{\"type\":\"rate_limit_error\"}}".to_vec(),
        });
        let adapter = AnthropicAdapter::new(transport, "claude-opus-5", "k");
        let error = block_on(adapter.complete(&call_a())).expect_err("429");
        match error {
            SummarizeError::Http { status, body } => {
                assert_eq!(status, 429);
                assert!(body.contains("rate_limit_error"));
                assert!(SummarizeError::Http { status, body }.is_retryable());
            }
            other => panic!("expected an HTTP error, got {other:?}"),
        }
    }

    #[test]
    fn citations_parse_into_segment_indices_and_thinking_blocks_are_skipped() {
        let body = json!({
            "content": [
                { "type": "thinking", "thinking": "let me look at segment 2" },
                {
                    "type": "text",
                    "text": "The migration script is owed by Friday.",
                    "citations": [{
                        "type": "content_block_location",
                        "document_index": 0,
                        "start_block_index": 2,
                        "end_block_index": 3,
                        "cited_text": "I will write the migration script by Friday."
                    }]
                },
                { "type": "text", "text": "No citation on this one." }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 120,
                "output_tokens": 40,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 18000
            }
        });

        let response = parse_response(body.to_string().as_bytes()).expect("parses");
        assert_eq!(
            response.blocks.len(),
            2,
            "the thinking block must not count"
        );
        assert_eq!(response.blocks[0].citations.len(), 1);
        assert_eq!(
            response.blocks[0].citations[0].segment_indices(),
            vec![2],
            "a citation must resolve to the segment index it names"
        );
        assert!(response.blocks[1].citations.is_empty());
        assert_eq!(response.usage.cache_read_input_tokens, 18_000);
        assert_eq!(response.usage.output_tokens, 40);
        assert!(response.ensure_complete().is_ok());
    }

    #[test]
    fn a_char_location_citation_is_not_mistaken_for_a_block_index() {
        // Character offsets and block indices are both integers. Treating a
        // char_location's offsets as segment ids would point every claim at a
        // confidently wrong timestamp.
        let body = json!({
            "content": [{
                "type": "text",
                "text": "x",
                "citations": [{
                    "type": "char_location",
                    "document_index": 0,
                    "start_char_index": 2,
                    "end_char_index": 3,
                    "cited_text": "x"
                }]
            }],
            "stop_reason": "end_turn"
        });
        let response = parse_response(body.to_string().as_bytes()).expect("parses");
        assert!(response.blocks[0].citations.is_empty());
    }

    #[test]
    fn a_malformed_response_is_a_decode_error_not_a_panic() {
        assert!(matches!(
            parse_response(b"not json"),
            Err(SummarizeError::Decode(_))
        ));
        assert!(matches!(
            parse_response(b"{\"id\":\"msg_1\"}"),
            Err(SummarizeError::Decode(_))
        ));
    }

    #[test]
    fn capabilities_come_from_the_descriptor_not_from_the_model_string() {
        let adapter = AnthropicAdapter::new(MockTransport::new(), "some-future-model", "k")
            .with_capabilities(Capabilities::local_default());
        assert!(!adapter.capabilities().native_citations);
        assert_eq!(adapter.model_id(), "some-future-model");
    }
}
