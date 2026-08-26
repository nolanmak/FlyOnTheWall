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
    /// Every engine there is.
    ///
    /// Enumerated rather than left to each caller's `match`, so a guard that
    /// has to answer "is this the name of a real CLI" cannot learn about one
    /// engine and not the other — see `refuse_test_egress` (#83). A third
    /// engine added to the enum and not to this array is a compile error at
    /// the array length, which is the point of writing it down.
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// What a bare enablement stores when the user names no binary.
    ///
    /// The **bare name**, and never a path — #87.
    ///
    /// `a0c40eb` probed the install directories here and froze the winner into
    /// the settings row. That bought reach: a bare name resolved fine from the
    /// user's shell, where `fotwd engine` runs, and then resolved to nothing
    /// inside a LaunchServices-launched `.app` whose PATH is
    /// `/usr/bin:/bin:/usr/sbin:/sbin` — the silent "configured but no
    /// summaries" trap. It also manufactured, one enablement at a time, every
    /// stale row [`probe`]'s basename rescue now exists to survive: the frozen
    /// path is wrong the moment the user upgrades node, moves a Homebrew
    /// prefix or reinstalls the CLI, and nothing ever rewrites it.
    ///
    /// [`resolve_binary`] answers "where is it" at *call* time now (#74), from
    /// inside the daemon, which is what made the freezing worth anything. So
    /// the row keeps the one fact that cannot go stale — which engine — and
    /// the daemon re-derives the rest on every pass.
    ///
    /// This is only the default. `--binary /some/path` is a different intent —
    /// *that* one, not whichever one you find — and is stored verbatim; see
    /// [`SummarizeSettings::binary`].
    #[must_use]
    pub fn default_binary(self) -> String {
        self.bare_name().to_owned()
    }

    /// The binary name this engine installs under.
    #[must_use]
    pub fn bare_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The `fotwd engine <sub>` that configures it.
    #[must_use]
    pub fn subcommand(self) -> &'static str {
        match self {
            Self::Claude => "claude-cli",
            Self::Codex => "codex-cli",
        }
    }

    /// KEY-04's disclosure for this engine, one line at a time.
    ///
    /// The words live in [`fotw_web::CliEngine`], not here, and that direction
    /// is deliberate: the dashboard's settings pane and `fotwd engine`'s
    /// confirmation prompt show the same disclosure, and `fotwd` is the crate
    /// that can see both. Two copies would be one copy that is eventually
    /// wrong about which host a transcript goes to, which is the one thing a
    /// disclosure must never be. Lines carry no leading indentation; each
    /// surface indents for itself.
    #[must_use]
    pub fn disclosure(self) -> &'static [&'static str] {
        fotw_web::CliEngine::from(self).disclosure()
    }
}

/// The wire spelling is shared, so the mapping is total and lossless in both
/// directions — and lives beside [`CliKind`] so a third engine cannot be added
/// on one side only.
impl From<CliKind> for fotw_web::CliEngine {
    fn from(kind: CliKind) -> Self {
        match kind {
            CliKind::Claude => Self::Claude,
            CliKind::Codex => Self::Codex,
        }
    }
}

impl From<fotw_web::CliEngine> for CliKind {
    fn from(engine: fotw_web::CliEngine) -> Self {
        match engine {
            fotw_web::CliEngine::Claude => Self::Claude,
            fotw_web::CliEngine::Codex => Self::Codex,
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
    /// The binary to run, as [`resolve_binary`] is given it.
    ///
    /// **What a bare enablement stores is the bare name** — `"claude"`,
    /// `"codex"` — because that is the only spelling that cannot go stale
    /// (#87). [`resolve_binary`] probes `$PATH` and the real install
    /// directories on every pass, so the row says *which* engine and the
    /// daemon says *where*, and a node upgrade or a moved Homebrew prefix
    /// costs the user nothing.
    ///
    /// A path here is a path the user chose — `--binary`, or the dashboard's
    /// field — and is used verbatim while it exists. A path that has stopped
    /// existing falls back to its basename rather than failing, which is what
    /// rescues the rows written before #87 by a build that froze the probe's
    /// answer at configure time.
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

impl Engine {
    /// The binary this engine runs, for the CLI engines.
    ///
    /// `None` for the API engine, which runs no subprocess. Exists so a status
    /// line can print the path the daemon *resolved* beside the string that was
    /// *configured* — see [`resolve_engine_detailed`].
    #[must_use]
    pub fn binary(&self) -> Option<&std::path::Path> {
        match self {
            Self::Anthropic { .. } => None,
            Self::ClaudeCli { binary } | Self::Codex { binary } => Some(binary),
        }
    }
}

/// What this machine's daemon would do, in the three cases that used to look
/// identical from outside (#74).
#[derive(Debug)]
pub enum EngineResolution {
    /// An engine, ready to build adapters from.
    Engine(Engine),
    /// Nobody has configured one: no API key, and no acknowledged CLI.
    /// Summaries stay local and nothing leaves the device.
    NoneConfigured,
    /// A CLI *is* configured and acknowledged, and this machine cannot find
    /// its binary. The string is what the settings row holds, because a status
    /// line that will not name the failing binary cannot be acted on.
    Unresolvable {
        /// The configured binary, verbatim from the settings row.
        configured: String,
    },
}

/// Pick the engine for this machine, and say which of the three states it is
/// in when there is none.
///
/// An Anthropic API key always wins — it is explicit configuration and keeps
/// the pre-CLI behavior byte-for-byte. With no key, the enabled-and-
/// acknowledged CLI serves, `cli_kind` deciding which. With neither,
/// enrichment stays local and nothing leaves the device.
///
/// An enabled-but-unacknowledged CLI reports as [`EngineResolution::NoneConfigured`],
/// not as unresolvable: without the acknowledgement nothing is configured yet,
/// and telling the user their binary is missing would send them to fix the
/// wrong thing.
#[must_use]
pub fn resolve_engine_detailed(store: &dyn KeyStore, db: &Db) -> EngineResolution {
    if let Ok(key) = store.get(SecretKey::ApiKey(Provider::Anthropic)) {
        return EngineResolution::Engine(Engine::Anthropic { key });
    }

    let settings = SummarizeSettings::read(db);
    if !settings.cli_enabled || !settings.acknowledged_egress || settings.binary.is_empty() {
        return EngineResolution::NoneConfigured;
    }
    // Resolved now rather than at call time: "no engine" at resolution is a
    // visible state the caller can report, where a spawn failure hours later
    // inside a finished meeting's enrichment is a surprise in a log.
    let Some(binary) = resolve_binary(&settings.binary) else {
        return EngineResolution::Unresolvable {
            configured: settings.binary,
        };
    };
    EngineResolution::Engine(match settings.cli_kind {
        CliKind::Claude => Engine::ClaudeCli { binary },
        CliKind::Codex => Engine::Codex { binary },
    })
}

/// [`resolve_engine_detailed`], for the callers that only need the engine.
#[must_use]
pub fn resolve_engine(store: &dyn KeyStore, db: &Db) -> Option<Engine> {
    match resolve_engine_detailed(store, db) {
        EngineResolution::Engine(engine) => Some(engine),
        EngineResolution::NoneConfigured | EngineResolution::Unresolvable { .. } => None,
    }
}

/// [`probe`] against this process's real `PATH` and `HOME`.
///
/// Public so `fotwd engine` can report what the *daemon* would resolve rather
/// than what the shell would — the two disagreeing is the whole of #74's
/// second mechanism.
///
/// This is the only function in the tree that hands [`probe`] the real
/// machine, and therefore the only door from a settings row to a binary that
/// will actually be spawned. That makes it the one place the #83 guard has to
/// stand; see `refuse_test_egress` below.
#[must_use]
pub fn resolve_binary(configured: &str) -> Option<PathBuf> {
    #[cfg(feature = "test-guards")]
    refuse_test_egress(configured);
    let path_var = std::env::var_os("PATH");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    probe(configured, path_var.as_deref(), home.as_deref())
}

/// Whether this process asked for real engines — #83's escape hatch.
///
/// Off unless `FOTW_ENGINE_LIVE=1`, the same shape and the same reasoning as
/// `fotw_secrets::os_tests_enabled`'s `FOTW_KEYCHAIN_TESTS`: a test that
/// spends someone's subscription runs because it was asked to, never because
/// it happened to be in the suite.
#[cfg(feature = "test-guards")]
#[must_use]
pub fn engine_live_opt_in() -> bool {
    std::env::var("FOTW_ENGINE_LIVE").is_ok_and(|value| value == "1")
}

/// Stop a test from resolving the developer's own `claude` or `codex` — #83.
///
/// # The hazard
///
/// [`probe`]'s basename rescue is correct in production and dangerous in
/// `cargo test`, for the same reason: it finds a real engine even when the
/// configured path is a fiction. A fixture that configures
/// `/no/such/place/claude` gets the dead directory stripped, the basename
/// probed against *this machine's* install spots, and the developer's real
/// `~/.local/bin/claude` handed back — which enrichment then spawns with a
/// transcript on stdin. That is a fixture leaving the machine from a test run.
/// It has happened once already, during #74's own development, and the only
/// thing that gave it away was the test taking 17 seconds.
///
/// Naming test fixtures `fotw-no-such-engine` avoids it by convention, and
/// convention is one plausible `dir.join("claude")` away from failing.
///
/// # What is refused, and what is not
///
/// Refused: a configured value whose basename is a real engine and which is
/// not itself an executable file — the rescue's exact precondition, and also
/// the bare `"claude"` a settings fixture might carry. Allowed: a path that
/// really is there (a test's own stub, used verbatim, never probed), and any
/// name no installer uses, such as [`crate::testing::UNRESOLVABLE_ENGINE`].
///
/// The pure [`probe`] is deliberately *not* guarded. It takes `PATH` and
/// `HOME` as arguments, so a test that hands it a `tempfile` home is
/// describing a machine rather than touching one, and `tests/engine.rs` pins
/// the whole of #74's candidate order that way.
///
/// # Getting past it on purpose
///
/// `FOTW_ENGINE_LIVE=1`, the same shape as `FOTW_CODEX_LIVE=1` in
/// `tests/codex_live.rs` and `FOTW_KEYCHAIN_TESTS=1` in `fotw-secrets`: a test
/// that means to spend someone's subscription says so out loud.
///
/// # Panics
///
/// That is the entire mechanism — a loud, immediate failure in place of a
/// silent egress.
#[cfg(feature = "test-guards")]
fn refuse_test_egress(configured: &str) {
    let path = std::path::Path::new(configured);
    // The configured path is really there, so `probe` returns it verbatim and
    // never looks at the basename. Every stub-planting test in the suite lives
    // here, and is safe by construction rather than by naming.
    if is_executable(path) {
        return;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if !CliKind::ALL.iter().any(|kind| kind.bare_name() == name) {
        return;
    }
    if engine_live_opt_in() {
        return;
    }
    panic!(
        "refusing to resolve `{configured}` from a test — #83.\n\n  \
         Its basename is the real `{name}` CLI and that path is not there, so \
         the #74 basename rescue would\n  \
         probe this machine's own install spots, find your `{name}`, and hand \
         it back for the caller to\n  \
         spawn — with whatever transcript the fixture is holding on its stdin.\n\n  \
         Configure `fotwd::testing::UNRESOLVABLE_ENGINE` for an engine no \
         machine can resolve, or plant a\n  \
         real stub named `fotwd::testing::STUB_ENGINE_NAME`. To drive the real \
         CLI on purpose, set\n  \
         FOTW_ENGINE_LIVE=1, the way `tests/codex_live.rs` gates on \
         FOTW_CODEX_LIVE=1."
    );
}

/// Find the binary a settings row names, the way `resolve_gh` does it
/// (`github.rs`): `$PATH` first, then the places the installers really use.
///
/// # Why this is not just `$PATH`
///
/// A LaunchServices-launched `.app` gets `PATH=/usr/bin:/bin:/usr/sbin:/sbin`
/// — confirmed with `ps eww` on a running daemon — and neither `claude` nor
/// `codex` installs into any of those four. So the bare name that resolves
/// perfectly from the user's shell, where `fotwd engine` runs and reports the
/// engine healthy, resolves to nothing inside the daemon that has to run it.
/// Probing at *call* time is what makes those two answers the same answer.
///
/// # Why a configured path still falls back to its basename
///
/// `a0c40eb` probed at configure time and froze the answer into the settings
/// row. A row written then still names wherever the binary lived that day.
/// Falling back to the basename rescues it — and rescues it from a *read*
/// path, which is why nothing is rewritten: `put_setting` bumps the settings
/// merge triple, and a read that silently wins merges against the user's other
/// laptop is a worse bug than the one being fixed.
///
/// # Why that rescue is a hazard in tests, and only in tests
///
/// The rescue's whole value is that it finds a real engine when the configured
/// path is wrong — which is precisely what makes it dangerous under
/// `cargo test`, where the configured path is *deliberately* wrong. A fixture
/// naming `/no/such/place/claude` has the dead directory stripped and gets the
/// developer's own `claude` back, and the caller spawns it with a transcript.
/// See #83, and `refuse_test_egress`, which stands at [`resolve_binary`] —
/// the only caller that hands this function the real machine.
///
/// It stands *there* and not here on purpose. Weakening this function is the
/// one thing #83 must not do: the rescue is the entire fix for #74 and the
/// reason summaries work at all on a LaunchServices-launched daemon. The
/// candidate order below is production behaviour, in tests and out.
///
/// Pure by construction — `path_var` and `home` are arguments — so the whole
/// candidate order is testable without mutating process-global environment,
/// and a test describing a machine is never a test touching one.
#[must_use]
pub fn probe(
    configured: &str,
    path_var: Option<&std::ffi::OsStr>,
    home: Option<&std::path::Path>,
) -> Option<PathBuf> {
    if configured.is_empty() {
        return None;
    }
    let name = if configured.contains(std::path::MAIN_SEPARATOR) {
        let direct = PathBuf::from(configured);
        // The user named a specific binary and it is there: never second-guess
        // that by probing.
        if is_executable(&direct) {
            return Some(direct);
        }
        std::path::Path::new(configured).file_name()?.to_owned()
    } else {
        std::ffi::OsString::from(configured)
    };

    let from_path = path_var
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default();
    from_path
        .into_iter()
        // An empty `PATH` element means "the current directory" to a shell.
        // For a daemon whose working directory is whatever LaunchServices
        // handed it, that is not a search path, it is an accident.
        .filter(|dir| !dir.as_os_str().is_empty())
        .chain(candidate_dirs(home))
        .map(|dir| dir.join(&name))
        .find(|candidate| is_executable(candidate))
}

/// Where the two CLIs actually install, searched after `$PATH`.
///
/// Also the tail of the `PATH` handed to the spawned child (see
/// [`TokioCliRunner`]), so the list is written once.
///
/// `~/.claude/local` is the claude installer's own private prefix, `~/.bun/bin`
/// is bun's, and `~/.nvm/versions/node/*/bin` is where an npm-installed
/// `claude` lands — a directory name nothing could guess, which is why it is
/// enumerated rather than assumed.
#[must_use]
pub fn candidate_dirs(home: Option<&std::path::Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    if let Some(home) = home {
        dirs.push(home.join(".claude/local"));
        dirs.push(home.join(".bun/bin"));
        dirs.extend(nvm_bin_dirs(home));
    }
    // codex also ships as an app bundle, which puts its binary somewhere no
    // `bin` directory convention would find.
    dirs.push(PathBuf::from("/Applications/Codex.app/Contents/Resources"));
    dirs
}

/// Every installed node's `bin`, newest first.
///
/// Sorted explicitly, and that is the point: `read_dir` order is unspecified,
/// so taking whatever came back first is a coin flip that could pin the daemon
/// to a node 16 install the user forgot they had.
fn nvm_bin_dirs(home: &std::path::Path) -> Vec<PathBuf> {
    let root = home.join(".nvm/versions/node");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut versions: Vec<(Vec<u64>, std::ffi::OsString)> = entries
        .flatten()
        .map(|e| (version_key(&e.file_name()), e.file_name()))
        .collect();
    // Descending, and numeric rather than lexical: `v9` sorts above `v22` as
    // text, which is exactly backwards.
    versions.sort_by(|a, b| b.cmp(a));
    versions
        .into_iter()
        .map(|(_, name)| root.join(name).join("bin"))
        .collect()
}

/// `v22.3.0` as `[22, 3, 0]`; anything unparseable as a zero, so a stray
/// directory sorts last instead of panicking.
fn version_key(name: &std::ffi::OsStr) -> Vec<u64> {
    name.to_string_lossy()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
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

/// The `PATH` the engine child gets: its own directory, then ours, then the
/// install spots [`candidate_dirs`] knows about.
///
/// # Why the binary's own directory comes first
///
/// An npm/nvm-installed `claude` is a `#!/usr/bin/env node` shim, and `node`
/// sits *beside* it in the same versioned directory. Resolving the shim's
/// absolute path — which is all [`probe`] does — does not help the shim find
/// its own interpreter: with the daemon's inherited
/// `PATH=/usr/bin:/bin:/usr/sbin:/sbin` the child dies with
/// `env: node: No such file or directory`, and the meeting silently gets no
/// summary. This one entry is that bug's fix (#74).
///
/// The inherited `PATH` follows, so a CLI that shells out to something the
/// user installed still finds it, and the probe's candidates come last so the
/// same list serves both "where do we look" and "where should the child look".
///
/// `HOME` here is deliberately the **real** home even for a shielded runner:
/// the CLIs are installed under the user's home, and the shield's job is to
/// hide `~`-relative *secrets* from an agentic child, not to make its own
/// installation unreachable.
fn child_path(binary: &std::path::Path) -> std::ffi::OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = binary.parent() {
        dirs.push(parent.to_path_buf());
    }
    dirs.extend(std::env::split_paths(&inherited));
    dirs.extend(candidate_dirs(home.as_deref()));

    let mut seen = std::collections::HashSet::new();
    dirs.retain(|dir| !dir.as_os_str().is_empty() && seen.insert(dir.clone()));

    // A directory containing the separator cannot be joined. Falling back to
    // the inherited PATH keeps the child no worse off than before; falling
    // back to an empty one would break every CLI that shells out.
    std::env::join_paths(dirs).unwrap_or(inherited)
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
                // …and a `PATH` the child can actually work with. Applied to
                // both arms: the shield below replaces `HOME`, never this.
                .env("PATH", child_path(&self.binary))
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
