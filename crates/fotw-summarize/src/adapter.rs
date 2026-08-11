//! The provider-agnostic LLM interface (spec 8.2).
//!
//! [`LlmRequest`] is what the pipeline builds and [`LlmAdapter`] is what turns
//! it into a provider's wire format. The pipeline never sees Anthropic's JSON,
//! and the adapter never decides what to ask for.
//!
//! **`citations` and `output_format` are both representable on the same
//! request, on purpose.** Making them a two-variant enum would render spec
//! 8.4's constraint unstatable — and a test asserting the pipeline never
//! combines them would then be vacuous, passing because the type system said
//! so rather than because the pipeline is right. Instead they are two fields,
//! [`LlmRequest::validate`] rejects the combination with the same error the API
//! would return, and `pipeline` proves against recorded request bodies that it
//! never constructs one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capabilities::{CacheTtl, Capabilities, Effort};
use crate::document::TranscriptDocument;
use crate::error::SummarizeError;
use crate::transport::BoxFuture;

/// The transcript document block and how it should be cached.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentPayload {
    /// The transcript.
    pub document: TranscriptDocument,
    /// Which segments to include, in order. The whole transcript for a
    /// single-shot call, one chunk's worth under map-reduce (spec 8.1).
    pub indices: Vec<usize>,
    /// Cache TTL for the prefix ending at this block.
    pub cache_ttl: CacheTtl,
    /// Title shown to the model.
    pub title: String,
}

/// One call to a model.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// Model id.
    pub model: String,
    /// The assembled system prompt (spec 8.3). Stable across meetings so it
    /// stays inside the cached prefix.
    pub system: String,
    /// The transcript, as a document block.
    pub document: Option<DocumentPayload>,
    /// The user's raw notes, as a text block after the document (spec 8.4).
    pub user_notes: Option<String>,
    /// The instruction block, last.
    pub instruction: String,
    /// Request server-side citations. Call A only.
    pub citations: bool,
    /// Request a strict JSON schema. Call B only.
    pub output_format: Option<Value>,
    /// `output_config.effort`, where supported.
    pub effort: Option<Effort>,
    /// Cap on the response.
    pub max_output_tokens: usize,
}

impl LlmRequest {
    /// Check the request against spec 8.4 and the adapter's capabilities.
    ///
    /// Called by every adapter before serializing. Cheap, and it converts a
    /// billed round trip ending in a 400 into a local error with a message
    /// that names the rule.
    ///
    /// # Errors
    ///
    /// [`SummarizeError::CitationsWithStructuredOutput`] for the mutually
    /// exclusive pair, [`SummarizeError::UnsupportedCapability`] when the
    /// request needs something the adapter does not offer.
    pub fn validate(&self, capabilities: &Capabilities) -> Result<(), SummarizeError> {
        if self.citations && self.output_format.is_some() {
            return Err(SummarizeError::CitationsWithStructuredOutput);
        }
        if self.citations && !capabilities.native_citations {
            return Err(SummarizeError::UnsupportedCapability {
                capability: "native_citations",
            });
        }
        if self.output_format.is_some() && !capabilities.strict_json_schema {
            return Err(SummarizeError::UnsupportedCapability {
                capability: "strict_json_schema",
            });
        }
        if self.effort.is_some() && !capabilities.supports_effort {
            return Err(SummarizeError::UnsupportedCapability {
                capability: "supports_effort",
            });
        }
        Ok(())
    }
}

/// A citation the provider attached to a response block (spec 8.4).
///
/// `start_block_index` is an index into the document's content blocks, which is
/// exactly [`crate::document::DocumentSegment::index`] — the property the whole
/// click-to-seek feature rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    /// First cited block, inclusive.
    pub start_block_index: usize,
    /// Last cited block, exclusive.
    pub end_block_index: usize,
    /// The cited span, extracted **by the API**, not by the model, and not
    /// billed as output tokens (spec 8.4).
    pub cited_text: String,
    /// Which document block the citation refers to. Always 0 here — the
    /// pipeline sends one document.
    pub document_index: usize,
}

impl Citation {
    /// The document segment indices this citation covers.
    #[must_use]
    pub fn segment_indices(&self) -> Vec<usize> {
        (self.start_block_index..self.end_block_index.max(self.start_block_index + 1)).collect()
    }
}

/// One text block of a response, with whatever citations rode along.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedBlock {
    /// The text.
    pub text: String,
    /// Citations attached to it. Empty is legal and is what
    /// [`crate::coverage`] measures.
    pub citations: Vec<Citation>,
}

/// Token accounting from a response.
///
/// SUM-11 shows the user real spend, so these come from the provider's `usage`
/// rather than from our own estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens.
    pub input_tokens: usize,
    /// Output tokens.
    pub output_tokens: usize,
    /// Tokens written to the prompt cache, billed at a premium.
    pub cache_creation_input_tokens: usize,
    /// Tokens served from the prompt cache at 0.1×. **Spec 8.4 says a zero
    /// here on Call B is a CI failure** — it means the prefix did not match and
    /// the pipeline is paying full price twice.
    pub cache_read_input_tokens: usize,
}

/// A model's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResponse {
    /// The response's text blocks in order.
    pub blocks: Vec<AnnotatedBlock>,
    /// Why generation stopped. SUM-10: check this before reading the content.
    pub stop_reason: Option<String>,
    /// Token accounting.
    pub usage: Usage,
}

impl LlmResponse {
    /// Every block's text, joined.
    #[must_use]
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Fail if the response was cut short (SUM-10).
    ///
    /// # Errors
    ///
    /// [`SummarizeError::Truncated`] when `stop_reason` indicates the model ran
    /// into a limit rather than finishing.
    pub fn ensure_complete(&self) -> Result<(), SummarizeError> {
        match self.stop_reason.as_deref() {
            Some("max_tokens") => Err(SummarizeError::Truncated(
                "hit max_tokens; the document is incomplete".to_string(),
            )),
            Some("refusal") => Err(SummarizeError::Truncated(
                "the model declined to answer".to_string(),
            )),
            _ => Ok(()),
        }
    }
}

/// A summarization provider.
///
/// Dyn-compatible (boxed futures rather than `async fn`) because the daemon
/// picks the adapter from user settings at runtime and stores it as
/// `Box<dyn LlmAdapter>`.
pub trait LlmAdapter: Send + Sync {
    /// What this provider can do. **Every branch downstream is on this, never
    /// on [`LlmAdapter::model_id`]** (spec 8.2).
    fn capabilities(&self) -> Capabilities;

    /// The model id, for logging and the meeting record. Not for branching.
    fn model_id(&self) -> &str;

    /// Run one call.
    fn complete<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> BoxFuture<'a, Result<LlmResponse, SummarizeError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::Capabilities;
    use crate::schema::EXTRACTION_SCHEMA;

    fn request() -> LlmRequest {
        LlmRequest {
            model: "test".to_string(),
            system: "system".to_string(),
            document: None,
            user_notes: None,
            instruction: "instruction".to_string(),
            citations: false,
            output_format: None,
            effort: None,
            max_output_tokens: 4_096,
        }
    }

    #[test]
    fn citations_plus_structured_output_is_rejected_locally() {
        // Spec 8.4: this pair is an HTTP 400. Catching it here turns a billed
        // round trip into an error naming the rule.
        let combined = LlmRequest {
            citations: true,
            output_format: Some(EXTRACTION_SCHEMA.clone()),
            ..request()
        };
        let error = combined
            .validate(&Capabilities::anthropic_frontier())
            .expect_err("must be rejected");
        assert!(matches!(
            error,
            SummarizeError::CitationsWithStructuredOutput
        ));
        assert!(error.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn either_feature_alone_is_fine() {
        let capabilities = Capabilities::anthropic_frontier();
        assert!(
            LlmRequest {
                citations: true,
                ..request()
            }
            .validate(&capabilities)
            .is_ok()
        );
        assert!(
            LlmRequest {
                output_format: Some(EXTRACTION_SCHEMA.clone()),
                ..request()
            }
            .validate(&capabilities)
            .is_ok()
        );
    }

    #[test]
    fn a_request_is_checked_against_capabilities_not_against_a_model_name() {
        // The spec 8.2 rule, enforced: a local model gets told which capability
        // it is missing, with no mention of who it is.
        let local = Capabilities::local_default();

        let error = LlmRequest {
            citations: true,
            ..request()
        }
        .validate(&local)
        .expect_err("local has no native citations");
        assert!(matches!(
            error,
            SummarizeError::UnsupportedCapability {
                capability: "native_citations"
            }
        ));

        let error = LlmRequest {
            output_format: Some(EXTRACTION_SCHEMA.clone()),
            ..request()
        }
        .validate(&local)
        .expect_err("local has no strict schema");
        assert!(matches!(
            error,
            SummarizeError::UnsupportedCapability {
                capability: "strict_json_schema"
            }
        ));

        let error = LlmRequest {
            effort: Some(Effort::High),
            ..request()
        }
        .validate(&local)
        .expect_err("local has no effort control");
        assert!(matches!(
            error,
            SummarizeError::UnsupportedCapability {
                capability: "supports_effort"
            }
        ));
    }

    #[test]
    fn a_truncated_response_is_an_error_before_its_content_is_read() {
        // SUM-10: a max_tokens stop leaves half a markdown document that would
        // otherwise render as if it were finished.
        let truncated = LlmResponse {
            blocks: vec![AnnotatedBlock {
                text: "## Decisions\n- We agreed to".to_string(),
                citations: Vec::new(),
            }],
            stop_reason: Some("max_tokens".to_string()),
            usage: Usage::default(),
        };
        assert!(truncated.ensure_complete().is_err());

        let complete = LlmResponse {
            stop_reason: Some("end_turn".to_string()),
            ..truncated.clone()
        };
        assert!(complete.ensure_complete().is_ok());
    }

    #[test]
    fn a_citation_resolves_to_the_segment_indices_it_spans() {
        let single = Citation {
            start_block_index: 4,
            end_block_index: 5,
            cited_text: "x".to_string(),
            document_index: 0,
        };
        assert_eq!(single.segment_indices(), vec![4]);

        let spanning = Citation {
            start_block_index: 4,
            end_block_index: 7,
            ..single.clone()
        };
        assert_eq!(spanning.segment_indices(), vec![4, 5, 6]);

        // Some responses report an end equal to the start. Treat it as the one
        // block rather than as an empty range, or a real citation resolves to
        // no segment and the claim reads as uncited.
        let degenerate = Citation {
            start_block_index: 4,
            end_block_index: 4,
            ..single
        };
        assert_eq!(degenerate.segment_indices(), vec![4]);
    }
}
