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

/// Which local CLI serves as the engine.
///
/// The wire spelling is `snake_case` and defaults to [`CliKind::Claude`] so a
/// settings row written before codex existed reads back as the claude engine
/// it was, never as an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliKind {
    /// The `claude` CLI, backed by a Claude subscription.
    #[default]
    Claude,
    /// The `codex` CLI, backed by a ChatGPT/Codex subscription.
    Codex,
}

impl CliKind {
    /// The binary a bare enablement defaults to when the user names none.
    #[must_use]
    pub fn default_binary(self) -> String {
        match self {
            Self::Claude => "claude".to_owned(),
            // codex ships as an app bundle and is usually reached through a
            // shell alias, which a daemon that spawns without a shell cannot
            // see. Prefer the app's real binary when it is where the
            // installer puts it, and fall back to the bare name for a PATH
            // install.
            Self::Codex => {
                const APP: &str = "/Applications/Codex.app/Contents/Resources/codex";
                if std::path::Path::new(APP).is_file() {
                    APP.to_owned()
                } else {
                    "codex".to_owned()
                }
            }
        }
    }
}

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
    /// Which CLI. Defaults to claude for rows written before codex existed.
    pub cli_kind: CliKind,
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
    /// The local `codex` CLI, BYO ChatGPT/Codex subscription.
    Codex {
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
            Self::Codex { binary } => f.debug_tuple("Engine::Codex").field(binary).finish(),
        }
    }
}

/// Pick the engine for this machine, or `None` when summaries stay local.
///
/// An Anthropic API key always wins — it is explicit configuration and keeps
/// the pre-CLI behavior byte-for-byte. With no key, the enabled-and-
/// acknowledged CLI serves, `cli_kind` deciding which. With neither,
/// enrichment stays local and nothing leaves the device.
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
    Some(match settings.cli_kind {
        CliKind::Claude => Engine::ClaudeCli { binary },
        CliKind::Codex => Engine::Codex { binary },
    })
}

/// A bare name searched on PATH; anything with a separator taken literally.
///
/// Public so `fotwd engine` can warn at configuration time when the binary it
/// just stored will not resolve — a spawn failure hours later inside a
/// finished meeting's enrichment is a far worse place to learn it.
pub fn resolve_binary(configured: &str) -> Option<PathBuf> {
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
/// not abandoned: an orphaned CLI process holds a subscription slot.
///
/// # The read shield, and why only codex needs it
///
/// `codex exec` is an *agentic* CLI: it runs model-generated shell commands on
/// its own, and its read-only sandbox bounds those to reads (no writes, no
/// network). A prompt injection inside the untrusted transcript (ING-11 — a
/// participant can say anything) could therefore make it `cat ~/.ssh/id_rsa`
/// and reflect the bytes into the summary. When [`TokioCliRunner::shielded`]
/// built this runner, the child gets an **empty `$HOME`** (a fresh temp dir)
/// so every `~`-relative secret path resolves to nothing, while `CODEX_HOME`
/// is pinned to the real `~/.codex` so the subscription login still works. The
/// `claude -p` path is not agentic and does not need this.
pub struct TokioCliRunner {
    binary: PathBuf,
    deadline: std::time::Duration,
    /// When present, the child's `$HOME` and working directory. Owned so it
    /// lives exactly as long as the runner — both pipeline calls share it and
    /// it is removed when the runner drops.
    shield: Option<tempfile::TempDir>,
}

impl TokioCliRunner {
    /// A runner for `binary`, giving each invocation `deadline` to finish.
    #[must_use]
    pub fn new(binary: PathBuf, deadline: std::time::Duration) -> Self {
        Self {
            binary,
            deadline,
            shield: None,
        }
    }

    /// [`TokioCliRunner::new`] with the read shield (see the type docs).
    ///
    /// Falls back to an unshielded runner — loudly — if the temp dir cannot be
    /// created: the read-only sandbox still blocks the child from sending
    /// anything out, so this is a narrower read radius, not the only control.
    #[must_use]
    pub fn shielded(binary: PathBuf, deadline: std::time::Duration) -> Self {
        match tempfile::TempDir::new() {
            Ok(shield) => Self {
                binary,
                deadline,
                shield: Some(shield),
            },
            Err(e) => {
                eprintln!("  ! could not create the codex read shield ({e}); running without it");
                Self::new(binary, deadline)
            }
        }
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
            let mut command = tokio::process::Command::new(&self.binary);
            command
                .args(argv)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                // The transcript is the child's stdin, not its environment;
                // a wiped environment also keeps DEEPGRAM_API_KEY and
                // friends out of a process that has no business seeing them.
                // OPENAI_API_KEY specifically: codex prefers it over the
                // subscription login when present, which would silently bill
                // the per-token API the CLI engine exists to avoid.
                .env_remove("DEEPGRAM_API_KEY")
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("OPENAI_API_KEY")
                .kill_on_drop(true);

            // The read shield: an empty $HOME so a prompt-injected
            // `cat ~/.ssh/...` inside an agentic CLI finds nothing, with
            // CODEX_HOME pinned to the real config dir so auth survives.
            if let Some(shield) = &self.shield {
                if let (None, Some(home)) =
                    (std::env::var_os("CODEX_HOME"), std::env::var_os("HOME"))
                {
                    command.env("CODEX_HOME", std::path::Path::new(&home).join(".codex"));
                }
                command.env("HOME", shield.path());
                command.current_dir(shield.path());
            }

            let mut child = command.spawn().map_err(|e| {
                SummarizeError::Transport(format!("could not start {}: {e}", self.binary.display()))
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
                        "{} hit its {}s deadline and was killed",
                        self.binary.display(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_secrets::{InMemoryKeyStore, Provider, SecretString};
    use fotw_store::{Db, DbKey};

    fn db() -> Db {
        Db::open_in_memory(&DbKey::from_bytes([0x01; 32])).unwrap()
    }

    fn enable(db: &mut Db, kind: CliKind, binary: &str) {
        let settings = SummarizeSettings {
            cli_enabled: true,
            acknowledged_egress: true,
            cli_kind: kind,
            binary: binary.to_owned(),
        };
        db.put_setting(SETTINGS_KEY, &serde_json::to_string(&settings).unwrap())
            .unwrap();
    }

    #[test]
    fn an_anthropic_key_always_wins_over_a_configured_cli() {
        let store = InMemoryKeyStore::new();
        store
            .set(
                SecretKey::ApiKey(Provider::Anthropic),
                &SecretString::new("sk-ant-xxx"),
            )
            .unwrap();
        let mut db = db();
        enable(&mut db, CliKind::Codex, "/bin/sh"); // an executable that exists
        assert!(
            matches!(resolve_engine(&store, &db), Some(Engine::Anthropic { .. })),
            "an explicit key is explicit configuration and must win"
        );
    }

    #[test]
    fn codex_is_chosen_when_it_is_the_configured_kind() {
        let store = InMemoryKeyStore::new();
        let mut db = db();
        // `/bin/sh` stands in for a real, resolvable binary on every unix CI.
        enable(&mut db, CliKind::Codex, "/bin/sh");
        assert!(matches!(
            resolve_engine(&store, &db),
            Some(Engine::Codex { .. })
        ));
    }

    #[test]
    fn the_claude_kind_still_resolves_to_the_claude_engine() {
        let store = InMemoryKeyStore::new();
        let mut db = db();
        enable(&mut db, CliKind::Claude, "/bin/sh");
        assert!(matches!(
            resolve_engine(&store, &db),
            Some(Engine::ClaudeCli { .. })
        ));
    }

    #[test]
    fn a_settings_row_from_before_codex_reads_back_as_claude() {
        // No `cli_kind` field at all — the shape written by the pre-codex
        // build. It must not fail to parse, and must mean claude.
        let mut db = db();
        db.put_setting(
            SETTINGS_KEY,
            r#"{"cli_enabled":true,"acknowledged_egress":true,"binary":"/bin/sh"}"#,
        )
        .unwrap();
        assert_eq!(SummarizeSettings::read(&db).cli_kind, CliKind::Claude);
        assert!(matches!(
            resolve_engine(&InMemoryKeyStore::new(), &db),
            Some(Engine::ClaudeCli { .. })
        ));
    }

    #[test]
    fn the_cli_is_refused_without_the_egress_acknowledgement() {
        let store = InMemoryKeyStore::new();
        let mut db = db();
        let settings = SummarizeSettings {
            cli_enabled: true,
            acknowledged_egress: false, // enabled, but not acknowledged
            cli_kind: CliKind::Codex,
            binary: "/bin/sh".to_owned(),
        };
        db.put_setting(SETTINGS_KEY, &serde_json::to_string(&settings).unwrap())
            .unwrap();
        assert!(
            resolve_engine(&store, &db).is_none(),
            "enablement without the acknowledgement is not enablement"
        );
    }
}
