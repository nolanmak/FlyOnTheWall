//! The `codex` CLI as an [`LlmAdapter`].
//!
//! The sibling of [`crate::claude_cli`], and for the same reason (#68): people
//! already paying for a ChatGPT/Codex subscription can spend it on this
//! workload instead of a second, per-token OpenAI API bill. It honours the
//! crate's founding rule — the seam is capabilities, not provider names — by
//! reporting truthfully what a CLI cannot do: no server-side citations, no
//! enforced JSON schema, no prompt cache. The pipeline already carries a
//! local path for every one of those (spec 8.2).
//!
//! # How this differs from the `claude` adapter, and why
//!
//! Two provider-shaped facts, nothing more:
//!
//! * **Invocation.** `codex exec -` runs non-interactively and reads the
//!   prompt from stdin (`-`), which is what keeps transcript content out of
//!   argv. `--json` makes stdout a machine-readable JSONL event stream;
//!   `--sandbox read-only` boxes any shell the model generates, because a
//!   summariser has no business writing the disk; `--skip-git-repo-check`,
//!   `--color never` and `--ephemeral` keep it silent, plain, and leaving no
//!   session files behind with the transcript in them.
//! * **Output.** There is no single result field. The answer is the *last*
//!   `agent_message` item in the stream; reasoning and command items are not
//!   messages, and a turn can carry several, so the final one wins.
//!
//! Everything else — the stdin block order, the transport seam, the capability
//! honesty — is shared with the `claude` adapter verbatim.

use std::sync::Arc;

use serde::Deserialize;

use crate::adapter::{AnnotatedBlock, LlmAdapter, LlmRequest, LlmResponse, Usage};
use crate::capabilities::{Capabilities, PromptCache};
use crate::claude_cli::{CliTransport, assemble_stdin};
use crate::error::SummarizeError;
use crate::transport::BoxFuture;

/// One line of `codex exec --json` output, parsed defensively.
///
/// Only the two shapes this adapter reads are named; every other event type
/// (and any non-JSON line) deserializes to a value we ignore. That tolerance
/// is exactly the deal [`Capabilities`] struck with `strict_json_schema`
/// false: nothing downstream may assume the stream is well-formed.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "item.completed")]
    ItemCompleted { item: Item },
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: Option<CodexUsage> },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: usize,
    #[serde(default)]
    output_tokens: usize,
}

/// [`LlmAdapter`] over a local `codex` binary.
pub struct CodexCliAdapter<T: CliTransport> {
    transport: Arc<T>,
    /// `-m <model>`, when the caller pins a tier. `None` uses the CLI's own
    /// configured default, which is the subscription's choice to make.
    model: Option<String>,
}

impl<T: CliTransport> CodexCliAdapter<T> {
    /// An adapter over `transport`, optionally pinning a model.
    pub fn new(transport: Arc<T>, model: Option<String>) -> Self {
        Self { transport, model }
    }

    fn argv(&self) -> Vec<String> {
        let mut argv = vec![
            "exec".to_owned(),
            // Read the prompt from stdin. The one rule that is not
            // negotiable: transcript content never rides in argv.
            "-".to_owned(),
            "--json".to_owned(),
            "--sandbox".to_owned(),
            "read-only".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "--color".to_owned(),
            "never".to_owned(),
            "--ephemeral".to_owned(),
        ];
        if let Some(model) = &self.model {
            // A model id names a tier; it is ours, not the transcript's, so
            // argv is exactly where it belongs.
            argv.push("-m".to_owned());
            argv.push(model.clone());
        }
        argv
    }
}

/// The last assistant message in the stream, and the usage if it was reported.
///
/// Separated from [`CodexCliAdapter::complete`] so the parse is testable as a
/// pure function of the bytes codex printed.
fn parse_stream(stdout: &str) -> (Option<String>, Usage) {
    let mut answer = None;
    let mut usage = Usage::default();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A line that is not JSON is not fatal: `--json` is best-effort and
        // the capability contract says so. Skip it and keep reading.
        let Ok(event) = serde_json::from_str::<Event>(line) else {
            continue;
        };
        match event {
            // The last agent message wins: a turn can emit several, and only
            // the final one is the answer. Reasoning and command items carry
            // a `text` too, which is why the kind is checked, not just text.
            Event::ItemCompleted { item } if item.kind == "agent_message" => {
                if let Some(text) = item.text {
                    answer = Some(text);
                }
            }
            Event::TurnCompleted { usage: Some(u) } => {
                usage = Usage {
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                    ..Usage::default()
                };
            }
            _ => {}
        }
    }
    (answer, usage)
}

impl<T: CliTransport> LlmAdapter for CodexCliAdapter<T> {
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
            // Conservative, matching the claude adapter: the real window is
            // larger, but planning against all of it leaves no room to answer.
            usable_context_tokens: 150_000,
            max_output_tokens: 32_000,
            supports_effort: false,
            supports_thinking: false,
        }
    }

    fn model_id(&self) -> &str {
        self.model.as_deref().unwrap_or("codex-cli")
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
                // Transport, not Http: the failure happened before any model
                // answer existed — same class as DNS, a reset, or "not
                // logged in". codex writes that explanation to stderr.
                return Err(SummarizeError::Transport(format!(
                    "codex CLI exited {}: {}",
                    output.status,
                    output.stderr.trim()
                )));
            }

            let (answer, usage) = parse_stream(&output.stdout);
            let Some(text) = answer else {
                return Err(SummarizeError::Decode(
                    "codex CLI produced no agent_message in its --json stream".to_owned(),
                ));
            };

            Ok(LlmResponse {
                blocks: vec![AnnotatedBlock {
                    text,
                    citations: Vec::new(),
                }],
                // The stream does not carry a stop reason; a completed turn
                // with an answer is an ordinary end, and SUM-10's check needs
                // a value.
                stop_reason: Some("end_turn".to_owned()),
                usage,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takes_the_last_agent_message_and_the_usage() {
        let jsonl = [
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"hmm"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"first"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"final"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":3}}"#,
        ]
        .join("\n");
        let (answer, usage) = parse_stream(&jsonl);
        assert_eq!(answer.as_deref(), Some("final"));
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 3);
    }

    #[test]
    fn parse_returns_none_when_no_message_was_emitted() {
        let (answer, _) = parse_stream(r#"{"type":"turn.started"}"#);
        assert!(answer.is_none());
    }
}
