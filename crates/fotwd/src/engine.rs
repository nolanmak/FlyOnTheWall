//! Which LLM engine a meeting gets — #68 — and the no-engine fallback — #67.
//!
//! # The resolution order is policy, not plumbing
//!
//! An Anthropic API key always wins: it is explicit configuration and keeps
//! the pre-CLI behavior byte-for-byte. With no key, the `claude` CLI serves —
//! but only when the user both enabled it and acknowledged that the
//! transcript leaves the machine. KEY-04's disclosure duty applies to this
//! path exactly as to a key: the words travel either way, only the bill
//! differs. With neither, enrichment degrades to a local fallback title and
//! nothing leaves the device at all.
//!
//! # Why the acknowledgement is stored, not inferred
//!
//! A `claude` binary on PATH is not consent. Half the machines that run this
//! tool have one for unrelated reasons, and a meeting transcript silently
//! flowing into it is the kind of surprise this project exists to not do.
//! `fotwd engine` records the acknowledgement in the settings row, and the
//! resolver refuses to use the CLI without it.

use std::path::PathBuf;

use fotw_secrets::{KeyStore, Provider, SecretKey, SecretString};
use fotw_store::Db;
use fotw_stt::transcript::TranscriptSegment;
use serde::{Deserialize, Serialize};

/// The `settings` key this lives under, beside `"github"` and `"retention"`.
pub const SETTINGS_KEY: &str = "summarize";

/// How long a fallback title may be, in bytes of UTF-8.
const TITLE_BUDGET: usize = 64;

/// The persisted CLI-engine choice.
///
/// Missing or unparseable reads as default — a fresh library, never an error
/// — the same shape as [`crate::github`] and retention settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizeSettings {
    /// The user turned the CLI engine on.
    pub cli_enabled: bool,
    /// The user was shown, and accepted, that transcripts leave the machine
    /// through the CLI. Enablement without this is not enablement.
    pub acknowledged_egress: bool,
    /// The binary to run. A bare name resolves on PATH; a path is used as-is.
    pub binary: String,
}

impl SummarizeSettings {
    /// The settings in force for this library.
    #[must_use]
    pub fn read(db: &Db) -> Self {
        db.get_setting(SETTINGS_KEY)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }
}

/// A resolved engine, ready to build adapters from.
pub enum Engine {
    /// The Anthropic HTTP API, BYO key.
    Anthropic {
        /// The key, read once from the credential store.
        key: SecretString,
    },
    /// The local `claude` CLI, BYO subscription.
    ClaudeCli {
        /// The resolved binary.
        binary: PathBuf,
    },
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key. §10's never-log rule does not stop at log files.
        match self {
            Self::Anthropic { .. } => f.write_str("Engine::Anthropic(<redacted>)"),
            Self::ClaudeCli { binary } => f.debug_tuple("Engine::ClaudeCli").field(binary).finish(),
        }
    }
}

/// Pick the engine for this machine, or `None` when summaries stay local.
#[must_use]
pub fn resolve_engine(store: &dyn KeyStore, db: &Db) -> Option<Engine> {
    if let Ok(key) = store.get(SecretKey::ApiKey(Provider::Anthropic)) {
        return Some(Engine::Anthropic { key });
    }

    let settings = SummarizeSettings::read(db);
    if !settings.cli_enabled || !settings.acknowledged_egress || settings.binary.is_empty() {
        return None;
    }
    // Resolved now rather than at call time: "no engine" at resolution is a
    // visible state the caller can report, where a spawn failure hours later
    // inside a finished meeting's enrichment is a surprise in a log.
    let binary = resolve_binary(&settings.binary)?;
    Some(Engine::ClaudeCli { binary })
}

/// A bare name searched on PATH; anything with a separator taken literally.
fn resolve_binary(configured: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(configured);
    if configured.contains(std::path::MAIN_SEPARATOR) {
        return is_executable(&direct).then_some(direct);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(configured))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// A title from the transcript alone, when no engine is configured (#67).
///
/// The first *substantive* utterance, trimmed at a word boundary. It will not
/// win awards; it beats `Untitled recording — 1787372240`, which is the bar.
#[must_use]
pub fn fallback_title(segments: &[TranscriptSegment]) -> Option<String> {
    let utterance = segments
        .iter()
        .map(|s| s.text.trim())
        // Grunts and acknowledgements make terrible titles; four words is
        // where speech starts saying something.
        .find(|t| t.split_whitespace().count() >= 4)?;

    if utterance.len() <= TITLE_BUDGET {
        return Some(utterance.to_owned());
    }
    // Cut at the last word boundary inside budget, never mid-word.
    let head = &utterance[..utterance
        .char_indices()
        .take_while(|(i, _)| *i < TITLE_BUDGET - 1)
        .last()
        .map_or(0, |(i, c)| i + c.len_utf8())];
    let cut = head.rfind(' ').unwrap_or(head.len());
    Some(format!("{}…", &head[..cut]))
}

/// The production [`CliTransport`]: a tokio process with a deadline.
///
/// The deadline is the same liveness argument as the keychain's: a CLI that
/// hangs — waiting on a login prompt nobody can see, or a network that went
/// away — must become an error the pipeline can report, never a meeting
/// enrichment that silently never finishes. On timeout the child is killed,
/// not abandoned: an orphaned `claude` process holds a subscription slot.
pub struct TokioCliRunner {
    binary: PathBuf,
    deadline: std::time::Duration,
}

impl TokioCliRunner {
    /// A runner for `binary`, giving each invocation `deadline` to finish.
    #[must_use]
    pub fn new(binary: PathBuf, deadline: std::time::Duration) -> Self {
        Self { binary, deadline }
    }
}

impl fotw_summarize::claude_cli::CliTransport for TokioCliRunner {
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        stdin: &'a str,
    ) -> fotw_summarize::transport::BoxFuture<
        'a,
        Result<fotw_summarize::claude_cli::CliOutput, fotw_summarize::error::SummarizeError>,
    > {
        use fotw_summarize::error::SummarizeError;
        Box::pin(async move {
            let mut child = tokio::process::Command::new(&self.binary)
                .args(argv)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                // The transcript is the child's stdin, not its environment;
                // a wiped environment also keeps DEEPGRAM_API_KEY and
                // friends out of a process that has no business seeing them.
                .env_remove("DEEPGRAM_API_KEY")
                .env_remove("ANTHROPIC_API_KEY")
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| {
                    SummarizeError::Transport(format!(
                        "could not start {}: {e}",
                        self.binary.display()
                    ))
                })?;

            // Write-then-close, so a CLI that reads to EOF gets its EOF.
            if let Some(mut handle) = child.stdin.take() {
                use tokio::io::AsyncWriteExt as _;
                handle.write_all(stdin.as_bytes()).await.map_err(|e| {
                    SummarizeError::Transport(format!("writing the prompt failed: {e}"))
                })?;
                drop(handle);
            }

            let waited = tokio::time::timeout(self.deadline, child.wait_with_output()).await;
            let output = match waited {
                Ok(result) => result.map_err(|e| {
                    SummarizeError::Transport(format!("collecting CLI output failed: {e}"))
                })?,
                Err(_) => {
                    // kill_on_drop reaps the child when `child` fell into
                    // wait_with_output; nothing left to kill by hand here.
                    return Err(SummarizeError::Transport(format!(
                        "claude CLI hit its {}s deadline and was killed",
                        self.deadline.as_secs_f64()
                    )));
                }
            };

            Ok(fotw_summarize::claude_cli::CliOutput {
                status: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    }
}
