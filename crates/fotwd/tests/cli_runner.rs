//! The production `TokioCliRunner`, and the read shield the codex engine
//! relies on (#68 review).
//!
//! No real `claude`/`codex` here — `/bin/sh` stands in for "a CLI", which is
//! all these assertions need: that stdin arrives, that a non-zero exit is
//! reported, and above all that a *shielded* runner hands the child an empty
//! `$HOME` so a prompt-injected `cat ~/.ssh/...` inside an agentic CLI finds
//! nothing.

#![cfg(unix)]

use std::time::Duration;

use fotw_summarize::claude_cli::CliTransport;
use fotwd::engine::TokioCliRunner;

fn argv(script: &str) -> Vec<String> {
    vec!["-c".to_owned(), script.to_owned()]
}

#[tokio::test]
async fn stdin_reaches_the_child_and_comes_back_out() {
    let runner = TokioCliRunner::new("/bin/sh".into(), Duration::from_secs(10));
    let out = runner
        .run(&argv("cat"), "the-prompt-body")
        .await
        .expect("sh runs");
    assert_eq!(out.status, 0);
    assert_eq!(out.stdout.trim(), "the-prompt-body");
}

#[tokio::test]
async fn a_nonzero_exit_is_reported_not_hidden() {
    let runner = TokioCliRunner::new("/bin/sh".into(), Duration::from_secs(10));
    let out = runner.run(&argv("exit 3"), "").await.expect("sh runs");
    assert_eq!(out.status, 3);
}

#[tokio::test]
async fn an_unshielded_runner_leaves_home_alone() {
    let runner = TokioCliRunner::new("/bin/sh".into(), Duration::from_secs(10));
    let out = runner
        .run(&argv("printf %s \"$HOME\""), "")
        .await
        .expect("sh runs");
    let real_home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(out.stdout, real_home, "no shield means the real HOME");
}

/// The shield's whole job: `$HOME` is a fresh empty dir, so `~`-relative
/// secret paths resolve to nothing. This is the control standing between an
/// agentic codex run and `~/.ssh/id_rsa`.
#[tokio::test]
async fn a_shielded_runner_redirects_home_away_from_the_real_one() {
    let runner = TokioCliRunner::shielded("/bin/sh".into(), Duration::from_secs(10));
    let out = runner
        .run(&argv("printf %s \"$HOME\""), "")
        .await
        .expect("sh runs");

    let real_home = std::env::var("HOME").unwrap_or_default();
    assert_ne!(
        out.stdout, real_home,
        "the shield must not be the real HOME"
    );
    assert!(!out.stdout.is_empty(), "HOME is set, just elsewhere");

    // And that elsewhere is empty: a `~`-relative read finds nothing.
    let listing = runner
        .run(&argv("ls -a \"$HOME\" | tr '\\n' ' '"), "")
        .await
        .expect("sh runs");
    let entries: Vec<&str> = listing
        .stdout
        .split_whitespace()
        .filter(|e| *e != "." && *e != "..")
        .collect();
    assert!(
        entries.is_empty(),
        "the shielded HOME must be empty, found: {entries:?}"
    );
}

/// The two runners share a process but not a HOME — proof the shield is per
/// runner, not a global mutation that would bleed into the claude path.
#[tokio::test]
async fn shielded_and_unshielded_runners_do_not_share_a_home() {
    let shielded = TokioCliRunner::shielded("/bin/sh".into(), Duration::from_secs(10));
    let plain = TokioCliRunner::new("/bin/sh".into(), Duration::from_secs(10));

    let a = shielded
        .run(&argv("printf %s \"$HOME\""), "")
        .await
        .unwrap();
    let b = plain.run(&argv("printf %s \"$HOME\""), "").await.unwrap();
    assert_ne!(a.stdout, b.stdout);
    assert_eq!(b.stdout, std::env::var("HOME").unwrap_or_default());
}

/// The `#!/usr/bin/env node` shim fix (#74), as a test.
///
/// An npm/nvm-installed `claude` is a shell script whose interpreter line is
/// `env node`, and `node` sits in the *same* directory. Resolving the shim's
/// absolute path does not help the shim find its interpreter: with the
/// daemon's `PATH=/usr/bin:/bin:/usr/sbin:/sbin` the child dies with
/// `env: node: No such file or directory` and the meeting silently gets no
/// summary. Putting the binary's own parent directory first on the child's
/// `PATH` is what fixes it.
///
/// Stood up with `/bin/sh` and a bare-named sibling rather than a real node:
/// the property under test is "a sibling of the binary resolves by bare name
/// inside the child", which is the same property either way.
#[tokio::test]
async fn the_child_can_resolve_a_bare_named_sibling_of_its_own_binary() {
    let (dir, binary) = shim_with_sibling();
    let runner = TokioCliRunner::new(binary, Duration::from_secs(10));

    let out = runner.run(&[], "").await.expect("the shim runs");

    assert_eq!(
        out.status, 0,
        "the sibling did not resolve: {:?}",
        out.stderr
    );
    assert_eq!(out.stdout.trim(), "sibling-ran");
    drop(dir);
}

/// The shield replaces `$HOME`; it does not take the child's `PATH` away.
/// Both arms need the interpreter fix, and the codex arm is the agentic one —
/// a runner that cannot start is a summary that never happens.
#[tokio::test]
async fn a_shielded_child_gets_the_path_fix_and_the_empty_home() {
    let (dir, binary) = shim_with_sibling();
    let runner = TokioCliRunner::shielded(binary, Duration::from_secs(10));

    let out = runner.run(&[], "").await.expect("the shim runs");
    assert_eq!(out.status, 0, "{:?}", out.stderr);
    assert_eq!(out.stdout.trim(), "sibling-ran");

    // And the shield is still a shield.
    let home = TokioCliRunner::shielded("/bin/sh".into(), Duration::from_secs(10))
        .run(&argv("printf %s \"$HOME\""), "")
        .await
        .unwrap();
    assert_ne!(home.stdout, std::env::var("HOME").unwrap_or_default());
    drop(dir);
}

/// The child's `PATH` is the binary's directory, then whatever this process
/// has, then the install spots the probe searches — so a CLI that shells out
/// to a tool the *user* installed still finds it.
#[tokio::test]
async fn the_child_path_puts_the_binary_beside_the_inherited_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let binary = write_script(dir.path(), "engine", "printf %s \"$PATH\"\n");
    let runner = TokioCliRunner::new(binary, Duration::from_secs(10));

    let out = runner.run(&[], "").await.expect("sh runs");
    let entries: Vec<&str> = out.stdout.split(':').collect();

    assert_eq!(
        entries.first().map(std::path::Path::new),
        Some(dir.path()),
        "the binary's own directory must come first: {:?}",
        out.stdout
    );
    for inherited in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        assert!(
            entries.iter().any(|e| std::path::Path::new(e) == inherited),
            "the inherited PATH must survive, missing {inherited:?}"
        );
    }
    assert!(
        entries.iter().any(|e| e.ends_with("/.local/bin")),
        "the probe's own candidates belong on the child's PATH too: {:?}",
        out.stdout
    );
}

/// The three provider keys are still removed. The `PATH` the child gained is
/// not a licence to hand it the rest of the environment — `OPENAI_API_KEY` in
/// particular, which codex prefers over the subscription login and which would
/// silently bill the per-token API the CLI engine exists to avoid.
#[tokio::test]
async fn the_provider_keys_are_still_stripped_from_the_child() {
    // Deliberately set here so the assertion is not vacuous on a machine that
    // has none of them. Nothing else in this file reads these.
    for key in ["DEEPGRAM_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        // SAFETY: single-threaded at this point in the test, and no other test
        // in this binary reads these variables.
        unsafe { std::env::set_var(key, "leaked") };
    }

    let runner = TokioCliRunner::new("/bin/sh".into(), Duration::from_secs(10));
    let out = runner
        .run(
            &argv(
                "printf '%s|%s|%s' \"${DEEPGRAM_API_KEY-}\" \"${ANTHROPIC_API_KEY-}\" \
                 \"${OPENAI_API_KEY-}\"",
            ),
            "",
        )
        .await
        .expect("sh runs");
    assert_eq!(out.stdout, "||", "a provider key reached the child engine");
}

/// A `binary` that resolves under a directory, plus a bare-named sibling it
/// invokes. Returns the temp dir (which must outlive the run) and the binary.
fn shim_with_sibling() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    write_script(dir.path(), "sibling-interpreter", "printf sibling-ran\n");
    let binary = write_script(dir.path(), "engine", "sibling-interpreter\n");
    (dir, binary)
}

fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
