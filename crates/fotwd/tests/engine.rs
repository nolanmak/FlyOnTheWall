//! Which LLM engine a meeting gets, and what happens with none — #67, #68.
//!
//! The order is a policy, pinned here: an Anthropic API key always wins
//! (today's behavior, untouched); with no key, the `claude` CLI serves — but
//! only when the user explicitly enabled it *and* acknowledged that the
//! transcript leaves the machine (KEY-04 applies to this path exactly as it
//! applies to a key: the words travel either way, only the bill differs).
//! With neither, enrichment degrades to a local fallback title and nothing
//! leaves the device.

use fotw_secrets::{InMemoryKeyStore, KeyStore, Provider, SecretKey, SecretString};
use fotw_stt::Source;
use fotw_stt::transcript::{TimestampSource, TranscriptSegment};
use fotwd::engine::{
    Engine, EngineResolution, SummarizeSettings, fallback_title, probe, resolve_binary,
    resolve_engine, resolve_engine_detailed,
};
use fotwd::testing::{UNRESOLVABLE_ENGINE, skip_if_engine_live};

fn db() -> fotw_store::Db {
    fotw_store::Db::open_in_memory(&key32()).unwrap()
}

fn key32() -> fotw_store::DbKey {
    fotw_store::DbKey::from_bytes([7u8; 32])
}

fn store_with_anthropic_key() -> InMemoryKeyStore {
    let store = InMemoryKeyStore::new();
    store
        .set(
            SecretKey::ApiKey(Provider::Anthropic),
            &SecretString::new("sk-ant-test"),
        )
        .unwrap();
    store
}

/// Settings rows the tests write directly, as `fotwd engine` would.
fn write_settings(db: &mut fotw_store::Db, settings: &SummarizeSettings) {
    db.put_setting("summarize", &serde_json::to_string(settings).unwrap())
        .unwrap();
}

fn cli_settings(binary: &str, acknowledged: bool) -> SummarizeSettings {
    SummarizeSettings {
        cli_enabled: true,
        acknowledged_egress: acknowledged,
        binary: binary.to_owned(),
        ..Default::default()
    }
}

// -------------------------------------------------------------- resolution

#[test]
fn no_key_and_no_cli_means_no_engine() {
    let db = db();
    assert!(resolve_engine(&InMemoryKeyStore::new(), &db).is_none());
}

#[test]
fn an_api_key_always_wins() {
    let mut db = db();
    // Even with the CLI fully enabled, the key is the explicit configuration
    // and keeps today's behavior byte-for-byte.
    write_settings(&mut db, &cli_settings("/bin/echo", true));

    match resolve_engine(&store_with_anthropic_key(), &db) {
        Some(Engine::Anthropic { .. }) => {}
        other => panic!("expected the API engine, got {other:?}"),
    }
}

#[test]
fn the_cli_serves_when_enabled_and_acknowledged_and_present() {
    let mut db = db();
    // /bin/echo exists on every machine this test runs on; the resolver only
    // asks whether the configured binary is executable, not whether it is
    // really the claude CLI — that is the invocation's job to discover.
    write_settings(&mut db, &cli_settings("/bin/echo", true));

    match resolve_engine(&InMemoryKeyStore::new(), &db) {
        Some(Engine::ClaudeCli { binary }) => {
            assert_eq!(binary, std::path::PathBuf::from("/bin/echo"));
        }
        other => panic!("expected the CLI engine, got {other:?}"),
    }
}

/// Enablement without the egress acknowledgement is not enablement. A
/// transcript silently flowing into whatever binary is on PATH is the kind
/// of surprise this project exists to not do.
#[test]
fn an_unacknowledged_cli_is_no_engine() {
    let mut db = db();
    write_settings(&mut db, &cli_settings("/bin/echo", false));
    assert!(resolve_engine(&InMemoryKeyStore::new(), &db).is_none());
}

#[test]
fn a_missing_binary_is_no_engine_rather_than_a_later_surprise() {
    let mut db = db();
    write_settings(&mut db, &cli_settings("/no/such/binary/anywhere", true));
    assert!(resolve_engine(&InMemoryKeyStore::new(), &db).is_none());
}

// --------------------------------------------------------- fallback titles

fn seg(start_ms: u64, text: &str) -> TranscriptSegment {
    TranscriptSegment {
        id: format!("{start_ms}"),
        session_id: "s".to_owned(),
        source: Source::System,
        speaker: None,
        text: text.to_owned(),
        start_ms,
        end_ms: start_ms + 1_000,
        words: Vec::new(),
        confidence: None,
        language: None,
        is_final: true,
        revision: 0,
        provider: "deepgram".to_owned(),
        model: "nova-3".to_owned(),
        timestamp_source: TimestampSource::Provider,
    }
}

/// With no engine, the first substantive utterance beats an epoch number.
#[test]
fn the_fallback_title_is_the_first_substantive_utterance() {
    let segs = vec![
        seg(0, "Um."),
        seg(
            1_000,
            "Okay so the interconnect bandwidth question from last week",
        ),
        seg(9_000, "right, exactly"),
    ];
    let title = fallback_title(&segs).expect("there is speech to draw from");
    assert!(
        title.starts_with("Okay so the interconnect"),
        "picked the wrong utterance: {title}"
    );
    assert!(title.len() <= 64, "a title is not a paragraph: {title:?}");
}

/// Short grunts do not become titles.
#[test]
fn noise_alone_yields_no_title() {
    assert!(fallback_title(&[seg(0, "Um."), seg(1, "yeah")]).is_none());
    assert!(fallback_title(&[]).is_none());
}

/// A long utterance is cut at a word boundary, not mid-word.
#[test]
fn a_long_utterance_is_trimmed_at_a_word_boundary() {
    let long = "the quarterly numbers look strong and the interconnect bandwidth \
                question from last week is now resolved so we can move forward";
    let title = fallback_title(&[seg(0, long)]).unwrap();
    assert!(title.len() <= 64);
    assert!(
        long.starts_with(title.trim_end_matches('…')),
        "the trim invented words: {title:?}"
    );
    assert!(!title.contains("bandwi…"), "mid-word cut: {title:?}");
}

// ------------------------------------------------------------- the probe

/// A throwaway `$HOME` with executables planted where the real installers put
/// them. The probe is pure — it takes `PATH` and `HOME` as arguments — so a
/// test can describe a whole machine without mutating this process's env.
struct FakeHome {
    dir: tempfile::TempDir,
}

impl FakeHome {
    fn new() -> Self {
        Self {
            dir: tempfile::TempDir::new().unwrap(),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Plant an executable at `rel` under this home and return its full path.
    fn install(&self, rel: &str) -> std::path::PathBuf {
        let path = self.dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

/// The bug in one test. A LaunchServices-launched `.app` gets
/// `PATH=/usr/bin:/bin:/usr/sbin:/sbin`; `claude` installs to
/// `~/.local/bin/claude`. Before #74 the daemon searched `$PATH` and nothing
/// else, so the bare name the user's shell resolves fine resolved to nothing
/// inside the daemon — and every meeting silently got a fallback title.
#[test]
fn a_bare_name_resolves_where_the_installer_actually_put_it() {
    let home = FakeHome::new();
    let installed = home.install(".local/bin/claude");

    let found = probe(
        "claude",
        Some(std::ffi::OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin")),
        Some(home.path()),
    );
    assert_eq!(found.as_deref(), Some(installed.as_path()));
}

/// Every spot the two CLIs really install into, one at a time. `~/.claude/local`
/// and `~/.bun/bin` are as real as Homebrew; an nvm-installed `claude` lives
/// under a versioned node directory that nothing else would guess.
#[test]
fn every_install_spot_the_daemon_cannot_see_on_path_is_probed() {
    for spot in [
        ".local/bin/claude",
        ".claude/local/claude",
        ".bun/bin/claude",
        ".nvm/versions/node/v22.3.0/bin/claude",
    ] {
        let home = FakeHome::new();
        let installed = home.install(spot);
        assert_eq!(
            probe("claude", Some(std::ffi::OsStr::new("")), Some(home.path())).as_deref(),
            Some(installed.as_path()),
            "{spot} must be probed"
        );
    }
}

/// `read_dir` order is unspecified, so "whichever nvm version came back
/// first" is a coin flip that would silently pin the daemon to node 16.
#[test]
fn the_newest_nvm_node_wins_rather_than_whatever_read_dir_returned() {
    let home = FakeHome::new();
    home.install(".nvm/versions/node/v18.20.4/bin/claude");
    home.install(".nvm/versions/node/v20.11.1/bin/claude");
    let newest = home.install(".nvm/versions/node/v22.3.0/bin/claude");

    assert_eq!(
        probe("claude", Some(std::ffi::OsStr::new("")), Some(home.path())).as_deref(),
        Some(newest.as_path())
    );
}

/// `PATH` is the user's own choice and comes first — the probe is a fallback
/// for a daemon with no PATH worth the name, never an override.
#[test]
fn path_still_wins_over_the_probed_directories() {
    let home = FakeHome::new();
    home.install(".local/bin/claude");
    let chosen = home.install("elsewhere/claude");

    let found = probe(
        "claude",
        Some(home.path().join("elsewhere").as_os_str()),
        Some(home.path()),
    );
    assert_eq!(found.as_deref(), Some(chosen.as_path()));
}

/// The rescue for the rows `a0c40eb` froze: it resolved the binary at
/// *configure* time and stored the absolute path, so a settings row written
/// while `claude` lived somewhere else still names that dead path. Falling
/// back to the basename finds the real one — without rewriting the settings
/// row, which would bump the settings merge triple from a read path.
#[test]
fn a_stale_absolute_path_falls_back_to_its_basename() {
    let home = FakeHome::new();
    let installed = home.install(".local/bin/claude");

    let found = probe(
        "/opt/homebrew/bin-that-was-removed/claude",
        Some(std::ffi::OsStr::new("")),
        Some(home.path()),
    );
    assert_eq!(found.as_deref(), Some(installed.as_path()));
}

/// A configured absolute path that still exists is used as-is, basename
/// probing never consulted: the user named a specific binary.
#[test]
fn a_configured_path_that_exists_is_used_verbatim() {
    let home = FakeHome::new();
    let chosen = home.install("custom/claude");
    home.install(".local/bin/claude");

    let found = probe(
        &chosen.to_string_lossy(),
        Some(std::ffi::OsStr::new("")),
        Some(home.path()),
    );
    assert_eq!(found.as_deref(), Some(chosen.as_path()));
}

#[test]
fn a_binary_that_exists_nowhere_resolves_to_nothing() {
    let home = FakeHome::new();
    assert!(probe("claude", Some(std::ffi::OsStr::new("")), Some(home.path())).is_none());
    assert!(probe("", Some(std::ffi::OsStr::new("")), Some(home.path())).is_none());
}

// ------------------------------------------ the test-egress guard — #83

/// The hazard, shown at the seam that leads to it and no further.
///
/// This is what a test that hand-writes `/no/such/place/claude` actually
/// gets: the #74 rescue strips the dead directory, probes the basename, and
/// hands back a real, runnable `claude` — which the caller then spawns with a
/// fixture transcript on stdin. That is an egress out of `cargo test`, and it
/// happened once already during #74's own development.
///
/// The `$HOME` here is a fake one, so this proves the *resolution* without
/// touching the developer's own install and without spawning anything. It
/// also pins the non-goal: the rescue itself must keep working exactly like
/// this, because it is the whole of #74.
#[test]
fn a_dead_claude_path_resolves_to_a_real_installed_claude() {
    let home = FakeHome::new();
    let installed = home.install(".local/bin/claude");

    let found = probe(
        "/no/such/place/claude",
        Some(std::ffi::OsStr::new("")),
        Some(home.path()),
    );
    assert_eq!(
        found.as_deref(),
        Some(installed.as_path()),
        "the rescue works as #74 intends — which is exactly why a test must \
         never reach it through the real machine's HOME and PATH"
    );
}

/// So the door that *does* read the real machine refuses instead.
///
/// [`resolve_binary`] is the only way a settings row becomes a spawnable
/// path, and it is the only entry point that consults this process's real
/// `HOME` and `PATH`. In a test build it declines the rescue's precondition —
/// a configured path that is not there, named after an engine that is — and
/// says so loudly. The pure [`probe`] above is untouched.
#[test]
#[should_panic(expected = "#83")]
fn resolving_a_dead_claude_path_against_the_real_machine_is_refused() {
    skip_if_engine_live();
    let _ = resolve_binary("/no/such/place/claude");
}

#[test]
#[should_panic(expected = "#83")]
fn resolving_a_dead_codex_path_against_the_real_machine_is_refused() {
    skip_if_engine_live();
    let _ = resolve_binary("/no/such/place/codex");
}

/// The acceptance criterion in the shape a test really writes it: a settings
/// row is the thing a fixture configures, and enrichment turns it into a
/// spawn two calls later. The refusal has to land here, before an `Engine`
/// exists to hand to a runner.
#[test]
#[should_panic(expected = "#83")]
fn an_engine_row_naming_a_dead_claude_never_becomes_a_spawnable_engine() {
    skip_if_engine_live();
    let mut db = db();
    write_settings(&mut db, &cli_settings("/no/such/place/claude", true));
    let _ = resolve_engine_detailed(&InMemoryKeyStore::new(), &db);
}

/// The guard is about the *rescue*, not about the word "claude". A path that
/// exists is used verbatim and never probed, so a test that plants its own
/// stub is safe by construction and must stay allowed — several already do.
#[test]
fn a_configured_path_that_exists_is_never_guarded() {
    assert_eq!(
        resolve_binary("/bin/echo").as_deref(),
        Some(std::path::Path::new("/bin/echo"))
    );
}

/// And the name the helper mints is not a hazard either: it resolves to
/// nothing on every machine, which is the whole point of minting it. This is
/// the assertion that keeps [`UNRESOLVABLE_ENGINE`] honest — a helper whose
/// path quietly started resolving would defeat every test that uses it.
#[test]
fn the_minted_unresolvable_engine_resolves_to_nothing_and_is_not_guarded() {
    assert!(resolve_binary(UNRESOLVABLE_ENGINE).is_none());
}

/// A settings row is not the only thing a fixture can carry: a bare `"claude"`
/// resolves off the developer's own `PATH` with no rescue involved at all.
/// That is a real engine reached from `cargo test` just the same, so the
/// refusal is about the *name*, not about the path having directories in it.
#[test]
#[should_panic(expected = "#83")]
fn a_bare_engine_name_is_refused_too_it_resolves_off_the_real_path() {
    skip_if_engine_live();
    let _ = resolve_binary("claude");
}

// -------------------------------------------------- the reported resolution

/// The state that used to be indistinguishable from "off". `fotwd engine`
/// and the dashboard both need to tell "nobody configured an engine" from
/// "somebody configured one and this machine cannot find it".
#[test]
fn nothing_configured_is_a_different_answer_from_configured_but_missing() {
    let fresh = db();
    assert!(matches!(
        resolve_engine_detailed(&InMemoryKeyStore::new(), &fresh),
        EngineResolution::NoneConfigured
    ));

    let mut db = db();
    write_settings(&mut db, &cli_settings("/no/such/binary/anywhere", true));
    match resolve_engine_detailed(&InMemoryKeyStore::new(), &db) {
        EngineResolution::Unresolvable { configured } => {
            assert_eq!(
                configured, "/no/such/binary/anywhere",
                "the status line has to name the binary that failed, or the \
                 user cannot fix it"
            );
        }
        other => panic!("expected Unresolvable, got {other:?}"),
    }
}

/// An enabled-but-unacknowledged CLI is *not* "unresolvable": nothing is
/// configured until the egress acknowledgement is there, and telling the user
/// their binary is missing would send them to fix the wrong thing.
#[test]
fn an_unacknowledged_cli_reports_as_nothing_configured() {
    let mut db = db();
    write_settings(&mut db, &cli_settings("/bin/echo", false));
    assert!(matches!(
        resolve_engine_detailed(&InMemoryKeyStore::new(), &db),
        EngineResolution::NoneConfigured
    ));
}

#[test]
fn a_resolvable_cli_reports_the_engine_it_resolved() {
    let mut db = db();
    write_settings(&mut db, &cli_settings("/bin/echo", true));
    match resolve_engine_detailed(&InMemoryKeyStore::new(), &db) {
        EngineResolution::Engine(Engine::ClaudeCli { binary }) => {
            assert_eq!(binary, std::path::PathBuf::from("/bin/echo"));
        }
        other => panic!("expected the CLI engine, got {other:?}"),
    }
}
