//! The `claude` CLI as an [`LlmAdapter`] (#68).
//!
//! Most people who would run this tool already pay for a Claude subscription
//! whose CLI serves exactly this workload; requiring an API key on top is a
//! second bill for the same model. The adapter honours this crate's founding
//! rule — the seam is capabilities, not provider names — by reporting
//! truthfully what a CLI cannot do: no server-side citations, no enforced
//! JSON schema, no prompt cache. The pipeline already carries a local path
//! for every one of those (spec 8.2), which is what makes this adapter
//! first-class rather than a hack.
//!
//! # Content never rides in argv
//!
//! The argument vector is readable by any same-user process, which is the
//! same reason the Deepgram key travels in a header and the recovery ceremony
//! refuses a key argument. Everything the model reads — system, document,
//! notes, instruction — goes over stdin, in spec 8.4's block order; argv
//! carries only flags and a model id.
//!
//! # Why a transport seam instead of `std::process`
//!
//! This crate does no IO of its own — HTTP arrives through an injected
//! [`crate::transport::HttpTransport`], and the CLI arrives the same way.
//! The real runner (a tokio process with a deadline) lives with the daemon,
//! which owns a runtime; the tests here need no binary at all.

use std::sync::Arc;

use serde::Deserialize;

use crate::adapter::{AnnotatedBlock, LlmAdapter, LlmRequest, LlmResponse, Usage};
use crate::capabilities::{Capabilities, PromptCache};
use crate::error::SummarizeError;
use crate::transport::BoxFuture;

/// What one CLI invocation produced.
#[derive(Debug, Clone)]
pub struct CliOutput {
    /// The process exit code.
    pub status: i32,
    /// Captured stdout — the JSON envelope on success.
    pub stdout: String,
    /// Captured stderr — where the CLI explains itself, e.g. "not logged in".
    pub stderr: String,
}

/// Anything that can run the CLI once.
///
/// `Send + Sync` because the pipeline holds adapters across awaits.
pub trait CliTransport: Send + Sync {
    /// Run the binary with `argv`, writing `stdin` to its stdin, and collect
    /// the result. Implementations own the deadline: a hung CLI must become
    /// an error, never a hung meeting pipeline.
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        stdin: &'a str,
    ) -> BoxFuture<'a, Result<CliOutput, SummarizeError>>;
}

/// The `-p --output-format json` envelope, parsed defensively.
///
/// Defensively because that is the deal [`Capabilities`] struck: with
/// `strict_json_schema` false, nothing downstream may assume well-formedness.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
}

/// [`LlmAdapter`] over a local `claude` binary.
pub struct ClaudeCliAdapter<T: CliTransport> {
    transport: Arc<T>,
    /// `--model`, when the caller picks a tier. `None` uses the CLI's own
    /// configured default, which is the subscription's choice to make.
    model: Option<String>,
}

impl<T: CliTransport> ClaudeCliAdapter<T> {
    /// An adapter over `transport`, optionally pinning a model.
    pub fn new(transport: Arc<T>, model: Option<String>) -> Self {
        Self { transport, model }
    }

    fn argv(&self) -> Vec<String> {
        let mut argv = vec![
            "-p".to_owned(),
            "--output-format".to_owned(),
            "json".to_owned(),
        ];
        if let Some(model) = &self.model {
            // The model id is a flag, not content: it names a tier, and it is
            // ours rather than the transcript's.
            argv.push("--model".to_owned());
            argv.push(model.clone());
        }
        argv
    }
}

/// The prompt as one stdin document, in spec 8.4's block order.
///
/// The API sends these as separate content blocks; a CLI has one stdin. The
/// order — system, document, notes, instruction — survives the flattening
/// because the quarantine reasoning behind it is about order, not transport:
/// the instruction stays last so nothing inside the transcript can pose as it.
///
/// `pub(crate)` because every CLI adapter feeds its subprocess the same one
/// stdin document — [`crate::codex_cli`] reuses this verbatim rather than
/// growing a second copy that could drift out of block order.
pub(crate) fn assemble_stdin(request: &LlmRequest) -> String {
    let mut out = String::with_capacity(4_096);
    out.push_str(&request.system);
    out.push_str("\n\n");

    if let Some(payload) = &request.document {
        out.push_str("<transcript title=\"");
        out.push_str(&payload.title.replace('"', "'"));
        out.push_str("\">\n");
        for index in &payload.indices {
            if let Some(segment) = payload.document.segment(*index) {
                out.push_str(&segment.block_text());
                out.push('\n');
            }
        }
        out.push_str("</transcript>\n\n");
    }

    if let Some(notes) = &request.user_notes {
        out.push_str("<user-notes>\n");
        out.push_str(notes);
        out.push_str("\n</user-notes>\n\n");
    }

    out.push_str(&request.instruction);
    out
}

impl<T: CliTransport> LlmAdapter for ClaudeCliAdapter<T> {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // A CLI cannot return server-side cited_text blocks; the pipeline
            // falls back to prompted [#N] markers (SUM-13).
            native_citations: false,
            // No response_format enforcement either: Call B parses and
            // validates locally.
            strict_json_schema: false,
            // Each invocation is a fresh process; there is no prefix to keep.
            prompt_cache: PromptCache::None,
            // Conservative: the CLI's context is the model's, but planning
            // against the full window leaves no room for the response.
            usable_context_tokens: 150_000,
            max_output_tokens: 32_000,
            supports_effort: false,
            supports_thinking: false,
        }
    }

    fn model_id(&self) -> &str {
        self.model.as_deref().unwrap_or("claude-cli")
    }

    fn complete<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> BoxFuture<'a, Result<LlmResponse, SummarizeError>> {
        Box::pin(async move {
            let argv = self.argv();
            let stdin = assemble_stdin(request);
            let output = self.transport.run(&argv, &stdin).await?;

            if output.status != 0 {
                // Transport, not Http: the failure happened before any
                // provider answer existed — same class as DNS or a reset.
                return Err(SummarizeError::Transport(format!(
                    "claude CLI exited {}: {}",
                    output.status,
                    output.stderr.trim()
                )));
            }

            let envelope: Envelope = serde_json::from_str(&output.stdout).map_err(|e| {
                SummarizeError::Decode(format!("claude CLI produced no JSON envelope: {e}"))
            })?;
            if envelope.is_error {
                return Err(SummarizeError::Transport(format!(
                    "claude CLI reported an error: {}",
                    envelope.result.unwrap_or_default()
                )));
            }
            let Some(text) = envelope.result else {
                return Err(SummarizeError::Decode(
                    "claude CLI envelope had no result field".to_owned(),
                ));
            };

            Ok(LlmResponse {
                blocks: vec![AnnotatedBlock {
                    text,
                    citations: Vec::new(),
                }],
                // The envelope does not carry one; "end_turn" is what a
                // successful result means, and SUM-10's check needs a value.
                stop_reason: Some("end_turn".to_owned()),
                usage: Usage::default(),
            })
        })
    }
}
