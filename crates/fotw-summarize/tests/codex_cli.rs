//! The `codex` CLI as an [`LlmAdapter`].
//!
//! The same bargain the `claude` CLI adapter struck (#68): people who already
//! pay for a ChatGPT/Codex subscription can spend it on this workload instead
//! of a second, per-token OpenAI API bill. It follows the crate's rule — "the
//! seam is capabilities, not provider names" — and #63's process pattern:
//! pinned argv, content on stdin, a transport seam so the tests need no real
//! binary.
//!
//! # The two rules that are not negotiable
//!
//! Transcript content never appears in argv (`ps` is world-readable to the
//! same user), and the model-generated shell codex may run is boxed to
//! `--sandbox read-only` — a summariser has no business writing the disk.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use fotw_summarize::adapter::{DocumentPayload, LlmAdapter, LlmRequest};
use fotw_summarize::capabilities::{CacheTtl, PromptCache};
use fotw_summarize::claude_cli::{CliOutput, CliTransport};
use fotw_summarize::codex_cli::CodexCliAdapter;
use fotw_summarize::document::TranscriptDocument;
use fotw_summarize::error::SummarizeError;
use fotw_summarize::pipeline::Pipeline;
use fotw_summarize::testing::{block_on, sample_meeting};
use fotw_summarize::transport::BoxFuture;

/// Records what the adapter asked for and answers with a canned result.
struct FakeCli {
    calls: Mutex<Vec<(Vec<String>, String)>>,
    /// One answer per invocation, in order.
    ///
    /// A queue rather than a single output because the two-call pipeline
    /// invokes the adapter twice and the two answers are different shapes.
    /// Running out is an error rather than a repeat, so a test that makes an
    /// unexpected extra call fails naming the call — the same bargain
    /// `MockTransport` strikes.
    outputs: Mutex<VecDeque<CliOutput>>,
}

impl FakeCli {
    fn answering(output: CliOutput) -> Arc<Self> {
        Self::scripted(vec![output])
    }

    fn scripted(outputs: Vec<CliOutput>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            outputs: Mutex::new(outputs.into()),
        })
    }

    fn ok(jsonl: &str) -> Arc<Self> {
        Self::answering(exit_zero(jsonl))
    }

    fn only_call(&self) -> (Vec<String>, String) {
        let calls = self.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "expected exactly one CLI invocation");
        calls[0].clone()
    }
}

fn exit_zero(stdout: &str) -> CliOutput {
    CliOutput {
        status: 0,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

impl CliTransport for FakeCli {
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        stdin: &'a str,
    ) -> BoxFuture<'a, Result<CliOutput, SummarizeError>> {
        self.calls
            .lock()
            .unwrap()
            .push((argv.to_vec(), stdin.to_owned()));
        let next = self.outputs.lock().unwrap().pop_front();
        Box::pin(async move {
            next.ok_or_else(|| {
                SummarizeError::Transport("FakeCli ran out of scripted answers".to_owned())
            })
        })
    }
}

const SYSTEM: &str = "SYSTEM-PROMPT-SENTINEL";
const NOTES: &str = "NOTES-SENTINEL decide on the rebinding guard";
const INSTRUCTION: &str = "INSTRUCTION-SENTINEL produce the summary";

fn request() -> LlmRequest {
    let document = TranscriptDocument::from_segments(&sample_meeting());
    let indices = document.all_indices();
    LlmRequest {
        model: String::new(),
        system: SYSTEM.to_owned(),
        document: Some(DocumentPayload {
            document,
            indices,
            cache_ttl: CacheTtl::None,
            title: "Quarterly planning".to_owned(),
        }),
        user_notes: Some(NOTES.to_owned()),
        instruction: INSTRUCTION.to_owned(),
        citations: false,
        output_format: None,
        effort: None,
        max_output_tokens: 4_096,
    }
}

/// One `agent_message` event carrying `text`, as `codex exec --json` emits it.
fn agent_message(text: &str) -> String {
    format!(
        r#"{{"type":"item.completed","item":{{"id":"item_0","type":"agent_message","text":{}}}}}"#,
        serde_json::to_string(text).unwrap()
    )
}

/// A realistic full stream: session, turn, the answer, usage.
fn stream(answer: &str) -> String {
    [
        r#"{"type":"thread.started","thread_id":"01a0"}"#.to_owned(),
        r#"{"type":"turn.started"}"#.to_owned(),
        agent_message(answer),
        r#"{"type":"turn.completed","usage":{"input_tokens":1200,"output_tokens":45}}"#.to_owned(),
    ]
    .join("\n")
}

// ------------------------------------------------------------ capabilities

#[test]
fn the_capabilities_are_honest_about_what_a_cli_cannot_do() {
    let adapter = CodexCliAdapter::new(FakeCli::ok(&stream("x")), None);
    let caps = adapter.capabilities();

    assert!(
        !caps.native_citations,
        "a CLI has no server-side cited_text"
    );
    assert!(!caps.strict_json_schema, "no response_format enforcement");
    assert_eq!(caps.prompt_cache, PromptCache::None);
    assert!(!caps.supports_effort);
}

// ------------------------------------------------------------------ the wire

#[test]
fn content_travels_on_stdin_and_never_in_argv() {
    let cli = FakeCli::ok(&stream("fine"));
    let adapter = CodexCliAdapter::new(Arc::clone(&cli), None);

    block_on(adapter.complete(&request())).expect("complete");
    let (argv, stdin) = cli.only_call();

    // Non-interactive exec, prompt from stdin (`-`), machine-readable events,
    // and a read-only sandbox so a summariser cannot touch the disk.
    assert_eq!(
        argv,
        [
            "exec",
            "-",
            "--json",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "--color",
            "never",
            "--ephemeral",
        ]
    );

    for sentinel in [SYSTEM, NOTES, INSTRUCTION] {
        assert!(stdin.contains(sentinel), "{sentinel} missing from stdin");
        assert!(
            !argv.join(" ").contains(sentinel),
            "{sentinel} leaked into argv, which any same-user process can read"
        );
    }
    assert!(
        stdin.contains("[#"),
        "document index markers missing: the local citation path needs them"
    );
}

#[test]
fn the_stdin_keeps_the_spec_block_order() {
    let cli = FakeCli::ok(&stream("fine"));
    let adapter = CodexCliAdapter::new(Arc::clone(&cli), None);

    block_on(adapter.complete(&request())).expect("complete");
    let (_, stdin) = cli.only_call();

    let at = |needle: &str| {
        stdin
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing"))
    };
    assert!(at(SYSTEM) < at("[#"));
    assert!(at("[#") < at(NOTES));
    assert!(at(NOTES) < at(INSTRUCTION));
}

#[test]
fn a_model_choice_is_argv_because_it_is_not_content() {
    let cli = FakeCli::ok(&stream("fine"));
    let adapter = CodexCliAdapter::new(Arc::clone(&cli), Some("gpt-5-codex".to_owned()));

    block_on(adapter.complete(&request())).expect("complete");
    let (argv, _) = cli.only_call();

    assert_eq!(&argv[argv.len() - 2..], ["-m", "gpt-5-codex"]);
    assert_eq!(adapter.model_id(), "gpt-5-codex");
}

// -------------------------------------------------------------- the answers

#[test]
fn the_final_agent_message_becomes_the_response_text() {
    let cli = FakeCli::ok(&stream("Decisions were made."));
    let adapter = CodexCliAdapter::new(cli, None);

    let response = block_on(adapter.complete(&request())).expect("complete");
    assert_eq!(response.text(), "Decisions were made.");
}

/// Codex can emit several agent messages in a turn; the last one is the
/// answer, and reasoning / other item types are not messages at all.
#[test]
fn the_last_agent_message_wins_and_non_messages_are_ignored() {
    let jsonl = [
        r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking..."}}"#.to_owned(),
        agent_message("a first partial thought"),
        r#"{"type":"item.completed","item":{"type":"command_execution","text":"ls"}}"#.to_owned(),
        agent_message("THE FINAL ANSWER"),
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#.to_owned(),
    ]
    .join("\n");
    let adapter = CodexCliAdapter::new(FakeCli::ok(&jsonl), None);

    let response = block_on(adapter.complete(&request())).expect("complete");
    assert_eq!(response.text(), "THE FINAL ANSWER");
}

/// Garbage interleaved with events must not abort the parse — `--json` is a
/// best-effort stream, and `strict_json_schema=false` is the promise that we
/// tolerate exactly this.
#[test]
fn non_json_lines_in_the_stream_are_skipped() {
    let jsonl = format!("a stray warning line\n{}\n", stream("Clean answer"));
    let adapter = CodexCliAdapter::new(FakeCli::ok(&jsonl), None);

    let response = block_on(adapter.complete(&request())).expect("complete");
    assert_eq!(response.text(), "Clean answer");
}

#[test]
fn a_nonzero_exit_is_a_transport_error_carrying_stderr() {
    let cli = FakeCli::answering(CliOutput {
        status: 1,
        stdout: String::new(),
        stderr: "stream error: not logged in — run `codex login`".to_owned(),
    });
    let adapter = CodexCliAdapter::new(cli, None);

    let err = block_on(adapter.complete(&request())).expect_err("must fail");
    match err {
        SummarizeError::Transport(msg) => assert!(msg.contains("not logged in")),
        other => panic!("expected Transport, got {other:?}"),
    }
}

// ------------------------------------------------------- the whole pipeline

/// An extraction whose evidence resolves against [`sample_meeting`].
fn extraction_json() -> String {
    serde_json::json!({
        "action_items": [{
            "text": "Write the migration script",
            "owner": "S0",
            "due": null,
            "due_raw": null,
            "confidence": "explicit",
            "evidence_segment_ids": [2],
            "evidence_quote": "I will write the migration script by Friday"
        }],
        "decisions": [], "open_questions": [], "follow_ups": [], "topics": []
    })
    .to_string()
}

/// #75's acceptance shape for codex: `parse_stream` strips the JSONL wrapper,
/// but the `agent_message` it leaves behind is a chatty assistant turn, not
/// bare JSON — and nothing was ever sent that could make it one.
#[test]
fn a_chatty_agent_message_with_fenced_json_still_yields_a_complete_summary() {
    let cli = FakeCli::scripted(vec![
        exit_zero(&stream(
            "The team agreed to move the storage layer to SQLite.",
        )),
        exit_zero(&stream(&format!(
            "Sure — here is the extraction:\n\n```json\n{}\n```\n\nLet me know if you want more.",
            extraction_json()
        ))),
    ]);
    let adapter = CodexCliAdapter::new(cli, None);
    let document = TranscriptDocument::from_segments(&sample_meeting());

    let pipeline = Pipeline::new(&adapter, &adapter);
    let outcome = block_on(pipeline.run(&document, "")).expect("chatter must not lose the summary");

    assert!(
        outcome.markdown().contains("SQLite"),
        "Call A's prose was lost"
    );
    assert_eq!(
        outcome.validation.extraction.action_items.len(),
        1,
        "the fenced extraction was not recovered"
    );
}

#[test]
fn a_stream_with_no_agent_message_is_a_decode_error() {
    // Exit 0, but the turn produced no assistant message at all.
    let jsonl = [
        r#"{"type":"thread.started","thread_id":"x"}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":0}}"#,
    ]
    .join("\n");
    let adapter = CodexCliAdapter::new(FakeCli::ok(&jsonl), None);

    let err = block_on(adapter.complete(&request())).expect_err("must fail");
    assert!(matches!(err, SummarizeError::Decode(_)));
}
