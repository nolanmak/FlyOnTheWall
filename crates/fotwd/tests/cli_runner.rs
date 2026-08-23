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
