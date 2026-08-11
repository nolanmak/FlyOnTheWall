//! `fotw-summarize` — transcript to grounded notes (spec 8).
//!
//! Takes `fotw_stt::TranscriptSegment`s and the user's raw notes, and returns
//! an augmented markdown document plus a structured extraction, both of which
//! have been checked against the transcript locally before anything reaches the
//! UI.
//!
//! # The shape of the thing
//!
//! ```text
//!   transcript segments
//!          │
//!          ▼
//!   document ──── one content block per segment, block index == evidence id
//!          │
//!          ├── Call A: citations ON,  no output format  ─► cited prose
//!          │                                               └─► coverage
//!          └── Call B: citations OFF, output format ON   ─► extraction
//!                                                          └─► validate
//! ```
//!
//! **Two calls, because citations and structured outputs are mutually
//! exclusive and sending both is an HTTP 400** (spec 8.4). Not a preference —
//! an API constraint that forces the architecture.
//!
//! # Three things worth knowing before changing anything here
//!
//! **The seam is capabilities, not provider names.** [`capabilities`] is the
//! only thing any branch in this crate looks at (spec 8.2). "Can this provider
//! do citations" is never spelled "is this Anthropic", which is what lets an
//! Ollama model take the same code path with weaker guarantees instead of a
//! parallel implementation.
//!
//! **The seam below is a trait, not an HTTP client.** This crate has no
//! sockets, no TLS and no async runtime: [`transport::HttpTransport`] is
//! injected. That is what makes every assertion in [`pipeline`] — including
//! "no request ever carries both mutually-exclusive features" — a claim about
//! recorded bytes, testable in a CI job that has no API key.
//!
//! **The strongest guarantee in here does not involve the model.**
//! [`validate`] is string arithmetic over what was actually said: it drops an
//! item whose cited segment does not exist or whose quote is not in it, and
//! nulls an owner or a date it cannot find. It behaves identically on every
//! provider and cannot be argued out of its answer by anything a participant
//! says on the call. Everything else — the prompt contract, the citations API —
//! is a request that a model may or may not honour.
//!
//! And the honest limit, from spec 8.6: **coverage of 1.0 does not mean zero
//! hallucination.** A model can cite a real segment while mischaracterizing it,
//! and a diarization error produces a correctly-cited item attributed to the
//! wrong person. The metric is called *transcript grounding* for that reason
//! and must never be relabelled "accuracy".
//!
//! See `docs/REQUIREMENTS.md` §8.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod adapter;
pub mod anthropic;
pub mod capabilities;
pub mod chunk;
pub mod coverage;
pub mod document;
pub mod error;
pub mod hash;
pub mod pipeline;
pub mod prompt;
pub mod schema;
pub mod testing;
pub mod tokens;
pub mod transport;
pub mod validate;

pub use adapter::{
    AnnotatedBlock, Citation, DocumentPayload, LlmAdapter, LlmRequest, LlmResponse, Usage,
};
pub use anthropic::AnthropicAdapter;
pub use capabilities::{CacheTtl, Capabilities, Effort, Preset, PromptCache};
pub use chunk::{Chunk, Plan, SpeakerTurn};
pub use coverage::{Claim, CoverageConfig, CoverageReport, LOW_GROUNDING_THRESHOLD, METRIC_LABEL};
pub use document::{DocumentSegment, TranscriptDocument};
pub use error::SummarizeError;
pub use pipeline::{Pipeline, PipelineConfig, SummaryOutcome, Warning, cache_ttl_for};
pub use prompt::{AUGMENT_PROMPT_VERSION, SystemPrompt, assemble};
pub use schema::{
    ActionItem, Confidence, Decision, EXTRACTION_SCHEMA, Extraction, FollowUp, ItemKind,
    OpenQuestion, Topic,
};
pub use tokens::estimate_tokens;
pub use transport::{HttpRequest, HttpResponse, HttpTransport};
pub use validate::{
    AdjustedItem, Adjustment, DropReason, DroppedItem, ValidationReport, ValidatorConfig, validate,
};
