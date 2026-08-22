//! The `claude` CLI as an [`LlmAdapter`] — issue #68.
//!
//! Most people who would run this tool already pay for a Claude subscription
//! whose CLI does exactly this workload; requiring an API key on top is a
//! second bill for the same model. The adapter follows the crate's own rule —
//! "the seam is capabilities, not provider names" — and the pattern #63
//! established for `gh`: pinned argv, content on stdin, a transport seam so
//! the tests need no real binary.
//!
//! # The one rule that is not negotiable
//!
//! Transcript content never appears in argv. The argument vector is readable
//! by any same-user process (`ps`), which is the same reason the Deepgram key
//! travels in a header and the recovery ceremony refuses a key argument. The
//! prompt — system, document, notes, instruction — goes over stdin, whole.

use std::sync::{Arc, Mutex};

use fotw_summarize::adapter::{DocumentPayload, LlmAdapter, LlmRequest};
use fotw_summarize::capabilities::{CacheTtl, PromptCache};
use fotw_summarize::claude_cli::{ClaudeCliAdapter, CliOutput, CliTransport};
use fotw_summarize::document::TranscriptDocument;
use fotw_summarize::testing::{block_on, sample_meeting};
use fotw_summarize::transport::BoxFuture;

/// Records what the adapter asked for and answers with a canned result.
struct FakeCli {
    calls: Mutex<Vec<(Vec<String>, String)>>,
    output: CliOutput,
}

impl FakeCli {
    fn answering(output: CliOutput) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            output,
        })
    }

    fn ok(result_json: &str) -> Arc<Self> {
        Self::answering(CliOutput {
            status: 0,
            stdout: result_json.to_owned(),
            stderr: String::new(),
        })
    }

    fn only_call(&self) -> (Vec<String>, String) {
        let calls = self.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "expected exactly one CLI invocation");
        calls[0].clone()
    }
}

impl CliTransport for FakeCli {
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        stdin: &'a str,
    ) -> BoxFuture<'a, Result<CliOutput, fotw_summarize::error::SummarizeError>> {
        self.calls
            .lock()
            .unwrap()
            .push((argv.to_vec(), stdin.to_owned()));
        let output = CliOutput {
            status: self.output.status,
            stdout: self.output.stdout.clone(),
            stderr: self.output.stderr.clone(),
        };
        Box::pin(async move { Ok(output) })
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

fn result_json(text: &str) -> String {
    format!(
        r#"{{"type":"result","is_error":false,"result":{}}}"#,
        serde_json::to_string(text).unwrap()
    )
}

// ------------------------------------------------------------ capabilities

/// The pipeline branches on these, never on the name (spec 8.2). The CLI has
/// no server-side citations and no schema enforcement, and saying otherwise
/// would route it down paths whose guarantees it cannot honour.
#[test]
fn the_capabilities_are_honest_about_what_a_cli_cannot_do() {
    let adapter = ClaudeCliAdapter::new(FakeCli::ok(&result_json("x")), None);
    let caps = adapter.capabilities();

    assert!(!caps.native_citations);
    assert!(!caps.strict_json_schema);
    assert_eq!(caps.prompt_cache, PromptCache::None);
    assert!(!caps.supports_effort);
}

// ------------------------------------------------------------------ the wire

#[test]
fn content_travels_on_stdin_and_never_in_argv() {
    let cli = FakeCli::ok(&result_json("fine"));
    let adapter = ClaudeCliAdapter::new(Arc::clone(&cli), None);

    block_on(adapter.complete(&request())).expect("complete");
    let (argv, stdin) = cli.only_call();

    assert_eq!(argv, ["-p", "--output-format", "json"]);

    for sentinel in [SYSTEM, NOTES, INSTRUCTION] {
        assert!(stdin.contains(sentinel), "{sentinel} missing from stdin");
        let joined = argv.join(" ");
        assert!(
            !joined.contains(sentinel),
            "{sentinel} leaked into argv, which any same-user process can read"
        );
    }
    // The transcript itself made the trip too.
    assert!(
        stdin.contains("[#"),
        "document index markers missing: the local citation path needs them \
         precisely because native_citations=false"
    );
}

/// Spec 8.4's block order — system, document, notes, instruction — holds on
/// stdin exactly as it holds in the API body, because the prompt contract's
/// quarantine reasoning is about order, not about transport.
#[test]
fn the_stdin_keeps_the_spec_block_order() {
    let cli = FakeCli::ok(&result_json("fine"));
    let adapter = ClaudeCliAdapter::new(Arc::clone(&cli), None);

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
    let cli = FakeCli::ok(&result_json("fine"));
    let adapter = ClaudeCliAdapter::new(Arc::clone(&cli), Some("claude-haiku-4-5".to_owned()));

    block_on(adapter.complete(&request())).expect("complete");
    let (argv, _) = cli.only_call();

    assert_eq!(
        argv,
        [
            "-p",
            "--output-format",
            "json",
            "--model",
            "claude-haiku-4-5"
        ]
    );
    assert_eq!(adapter.model_id(), "claude-haiku-4-5");
}

// -------------------------------------------------------------- the answers

#[test]
fn the_result_field_becomes_the_response_text() {
    let cli = FakeCli::ok(&result_json("Decisions were made."));
    let adapter = ClaudeCliAdapter::new(cli, None);

    let response = block_on(adapter.complete(&request())).expect("complete");
    assert_eq!(response.text(), "Decisions were made.");
    assert!(
        response.stop_reason.is_some(),
        "SUM-10 checks this before reading"
    );
}

#[test]
fn a_nonzero_exit_is_an_error_that_names_stderr() {
    let cli = FakeCli::answering(CliOutput {
        status: 1,
        stdout: String::new(),
        stderr: "not logged in".to_owned(),
    });
    let adapter = ClaudeCliAdapter::new(cli, None);

    let err = block_on(adapter.complete(&request())).expect_err("must fail");
    assert!(
        err.to_string().contains("not logged in"),
        "the user's fix is in stderr: {err}"
    );
}

/// The CLI reports its own failures inside a zero-exit JSON envelope.
#[test]
fn an_is_error_result_is_an_error_not_a_summary() {
    let cli = FakeCli::ok(r#"{"type":"result","is_error":true,"result":"usage limit reached"}"#);
    let adapter = ClaudeCliAdapter::new(cli, None);

    let err = block_on(adapter.complete(&request())).expect_err("must fail");
    assert!(err.to_string().contains("usage limit"), "{err}");
}

/// Defensive parsing is the deal capabilities struck: strict_json_schema is
/// false, so nothing downstream may assume the envelope is well-formed.
#[test]
fn malformed_output_is_an_error_rather_than_an_empty_summary() {
    let cli = FakeCli::ok("this is not json at all");
    let adapter = ClaudeCliAdapter::new(cli, None);

    assert!(block_on(adapter.complete(&request())).is_err());
}
