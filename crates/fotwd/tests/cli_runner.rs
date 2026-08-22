//! The tokio-backed [`CliTransport`] — the half of #68 that owns real IO.
//!
//! `fotw-summarize` does no IO; its adapter speaks to a seam. This is the
//! seam's production implementation, tested the way #63 tests `gh`: against
//! a fake binary that records what it was given, so the argv/stdin contract
//! is pinned at the process boundary and no real CLI is needed.

use std::time::{Duration, Instant};

use fotw_summarize::claude_cli::CliTransport;
use fotwd::engine::TokioCliRunner;

fn fake_cli(name: &str, script_body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("fotw-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("claude");
    std::fs::write(&bin, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bin, dir)
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn argv_and_stdin_reach_the_process_and_stdout_comes_back() {
    let (bin, dir) = fake_cli(
        "roundtrip",
        r#"cat > "$(dirname "$0")/stdin.txt"
printf '%s\n' "$@" > "$(dirname "$0")/argv.txt"
printf '{"type":"result","is_error":false,"result":"summarised"}'"#,
    );
    let runner = TokioCliRunner::new(bin, Duration::from_secs(10));

    let out = runner
        .run(
            &argv(&["-p", "--output-format", "json"]),
            "THE-PROMPT on stdin",
        )
        .await
        .expect("run succeeds");

    assert_eq!(out.status, 0);
    assert!(out.stdout.contains("summarised"));
    assert_eq!(
        std::fs::read_to_string(dir.join("stdin.txt")).unwrap(),
        "THE-PROMPT on stdin"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("argv.txt")).unwrap(),
        "-p\n--output-format\njson\n"
    );
}

#[tokio::test]
async fn a_nonzero_exit_reports_status_and_stderr() {
    let (bin, _dir) = fake_cli(
        "fails",
        r#"echo "not logged in" >&2
exit 3"#,
    );
    let runner = TokioCliRunner::new(bin, Duration::from_secs(10));

    let out = runner
        .run(&argv(&["-p"]), "x")
        .await
        .expect("captured, not Err");
    assert_eq!(out.status, 3);
    assert!(out.stderr.contains("not logged in"));
}

/// A hung CLI must become an error, never a hung meeting pipeline — the same
/// liveness argument as the keychain deadline, with the same shape of fix.
#[tokio::test]
async fn a_hung_binary_is_killed_at_the_deadline() {
    let (bin, _dir) = fake_cli("hangs", "sleep 30");
    let runner = TokioCliRunner::new(bin, Duration::from_millis(400));

    let began = Instant::now();
    let err = runner
        .run(&argv(&["-p"]), "x")
        .await
        .expect_err("must time out");

    assert!(
        began.elapsed() < Duration::from_secs(5),
        "the deadline did not fire: {:?}",
        began.elapsed()
    );
    assert!(
        err.to_string().contains("deadline") || err.to_string().contains("timed out"),
        "the error should say what happened: {err}"
    );
}
