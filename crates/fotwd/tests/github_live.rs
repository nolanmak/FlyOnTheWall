//! The one test that runs the real `gh` against a real repository.
//!
//! Self-skipping, exactly as the real-keychain tests in `fotw-secrets` are:
//! CI has no GitHub login, and a test that pushes to somebody's repository
//! must never run by accident. Opt in with a scratch repository you own:
//!
//! ```sh
//! FOTW_GH_LIVE=owner/scratch-repo cargo test -p fotwd --test github_live -- --nocapture
//! ```
//!
//! It commits a fixture transcript, then commits it again, proving the
//! create path, the sha probe, and the update path against GitHub's actual
//! answers rather than our transcript of them.

use std::sync::Arc;

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotw_web::GithubExport;
use fotwd::github::{GhRunner, GithubExporter, SETTINGS_KEY, SystemGh};

#[test]
fn a_real_push_lands_and_a_repush_updates_the_same_file() {
    let Some(repo) = std::env::var("FOTW_GH_LIVE").ok().filter(|r| !r.is_empty()) else {
        eprintln!("skipped: set FOTW_GH_LIVE=owner/repo (a scratch repo you own) to run");
        return;
    };

    let mut db = Db::open_in_memory(&DbKey::from_bytes([0x01; 32])).unwrap();
    let meeting = db
        .meetings()
        .create(
            NewMeeting::new("dev-1", "UTC")
                .title("Live smoke test")
                .started_at_ms(1_755_734_400_000),
        )
        .unwrap();
    let tid = db
        .meetings()
        .create_transcript(&meeting, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(
            &tid,
            &[NewSegment::new(
                0,
                0,
                1_500,
                "This sentence was pushed by the fotwd live smoke test.",
            )],
        )
        .unwrap();
    db.meetings().set_state(&meeting, "ready").unwrap();
    db.put_setting(
        SETTINGS_KEY,
        &format!(
            r#"{{"enabled":true,"repo":"{repo}","branch":"","path_prefix":"fotw-qa/","mode":"manual"}}"#
        ),
    )
    .unwrap();

    let dir = tempfile::TempDir::new().unwrap();
    let exporter = GithubExporter::new(
        db,
        dir.path().join("sessions"),
        Arc::new(SystemGh) as Arc<dyn GhRunner>,
    );

    let first = exporter.push(&meeting).expect("the live push lands");
    eprintln!("pushed {}/{} @ {}", first.repo, first.path, first.commit);
    assert_eq!(first.repo, repo);
    assert!(
        first
            .path
            .starts_with("fotw-qa/2025-08-21-live-smoke-test-")
    );
    assert_ne!(first.commit, "unknown", "GitHub answers with a commit sha");

    let second = exporter.push(&meeting).expect("the update lands");
    eprintln!(
        "updated {}/{} @ {}",
        second.repo, second.path, second.commit
    );
    assert_eq!(second.path, first.path, "same meeting, same file");
    assert_ne!(second.commit, first.commit, "an update is a new commit");

    // The OKF bundle: index.md and log.md, committed for real.
    exporter.sync_bundle().expect("the live bundle sync lands");
    eprintln!("synced fotw-qa/index.md and fotw-qa/log.md");
}
