//! What happens to a meeting the moment it finalizes — #67, #68.
//!
//! Enrichment never fails the meeting: the recording and transcript are
//! already safe on disk, and everything here is derived. Problems are
//! reported, not thrown — the same posture as `stt_errors`.

use fotw_secrets::InMemoryKeyStore;
use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotwd::engine::SummarizeSettings;
use fotwd::enrich::enrich_meeting_with;

fn db() -> Db {
    Db::open_in_memory(&DbKey::from_bytes([9u8; 32])).unwrap()
}

/// A meeting with a real transcript and the timestamp fallback title.
fn meeting_with_transcript(db: &mut Db) -> String {
    let mut m = NewMeeting::new("dev-1", "UTC");
    m.title = "Untitled recording — 1787372240".to_owned();
    let meeting = db.meetings().create(m).unwrap();
    let transcript = db
        .meetings()
        .create_transcript(&meeting, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(
            &transcript,
            &[
                NewSegment::new(0, 0, 900, "Um.").channel("system"),
                NewSegment::new(
                    1,
                    1_000,
                    4_000,
                    "Okay so the interconnect bandwidth question",
                )
                .channel("system"),
                NewSegment::new(2, 5_000, 6_000, "makes sense to me").channel("mic"),
            ],
        )
        .unwrap();
    meeting
}

fn cli_settings(db: &mut Db, binary: &str) {
    let settings = SummarizeSettings {
        cli_enabled: true,
        acknowledged_egress: true,
        binary: binary.to_owned(),
        ..Default::default()
    };
    db.put_setting("summarize", &serde_json::to_string(&settings).unwrap())
        .unwrap();
}

fn failing_cli(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("fotw-enrich-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("claude");
    std::fs::write(&bin, "#!/bin/sh\necho 'usage limit reached' >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin.to_string_lossy().into_owned()
}

/// No engine: the meeting still stops being an epoch number. Nothing leaves
/// the machine, and no summary row appears — but the *absence* is now
/// reported rather than skipped.
///
/// This assertion is inverted from what it was. Before #74 the no-engine case
/// pushed nothing onto `problems`, which made "engine off" produce zero
/// diagnostics by construction — the state that let 33 meetings go
/// unsummarised without a word anywhere the user could see.
#[tokio::test]
async fn without_an_engine_the_fallback_title_lands_and_nothing_egresses() {
    let mut db = db();
    let meeting = meeting_with_transcript(&mut db);

    let report = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &meeting).await;

    let row = db.meetings().get(&meeting).unwrap();
    assert_eq!(row.title, "Okay so the interconnect bandwidth question");
    assert_eq!(report.title.as_deref(), Some(row.title.as_str()));
    assert!(report.summary_version.is_none(), "no engine, no summary");

    let problem = report
        .problems
        .first()
        .unwrap_or_else(|| panic!("no engine must be reported: {:?}", report.problems));
    // The same string prints on `fotwd record`'s stderr, where "open Settings"
    // is advice about a window the user did not open. Both remedies, always.
    assert!(
        problem.contains("Settings") && problem.contains("fotwd engine"),
        "the copy must name both remedies: {problem:?}"
    );

    assert_eq!(row.enrich_status.as_deref(), Some("no_engine"));
    assert_eq!(row.enrich_detail, None, "there is no binary to blame");
}

/// The state that used to be indistinguishable from "off": a CLI is
/// configured and acknowledged, and *this daemon* cannot find it. The
/// configured string is persisted, because a report that will not name the
/// binary cannot be acted on.
///
/// The binary is named `fotw-no-such-engine` rather than `claude` on purpose.
/// A dead path whose *basename* resolves is the stale-row rescue working, and
/// this test would then run the developer's real CLI — sending a fixture
/// transcript to a provider from `cargo test`.
#[tokio::test]
async fn an_engine_the_daemon_cannot_resolve_is_reported_by_name() {
    let mut db = db();
    let meeting = meeting_with_transcript(&mut db);
    cli_settings(&mut db, "/no/such/place/fotw-no-such-engine");

    let report = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &meeting).await;

    let row = db.meetings().get(&meeting).unwrap();
    assert_eq!(
        row.title, "Okay so the interconnect bandwidth question",
        "an unresolvable engine still gets the fallback title"
    );
    assert_eq!(row.enrich_status.as_deref(), Some("engine_unresolvable"));
    assert_eq!(
        row.enrich_detail.as_deref(),
        Some("/no/such/place/fotw-no-such-engine")
    );
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.contains("/no/such/place/fotw-no-such-engine")),
        "the report must name the binary that failed: {:?}",
        report.problems
    );
}

/// A title the user typed is never overwritten. Only the timestamp fallback
/// is fair game — re-running enrichment must not undo a human's rename.
#[tokio::test]
async fn a_human_title_is_never_replaced() {
    let mut db = db();
    let meeting = meeting_with_transcript(&mut db);
    db.meetings().set_title(&meeting, "Panga kickoff").unwrap();

    enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &meeting).await;

    assert_eq!(db.meetings().get(&meeting).unwrap().title, "Panga kickoff");
}

/// A configured engine that fails at run time degrades to exactly the
/// no-engine outcome, with the failure reported rather than swallowed —
/// the lesson `StreamEvent::Error` taught this project.
#[tokio::test]
async fn a_failing_engine_still_yields_the_fallback_title_and_says_why() {
    let mut db = db();
    let meeting = meeting_with_transcript(&mut db);
    let bin = failing_cli("limit");
    cli_settings(&mut db, &bin);

    let report = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &meeting).await;

    assert_eq!(
        db.meetings().get(&meeting).unwrap().title,
        "Okay so the interconnect bandwidth question",
        "the engine failing must not leave the epoch title"
    );
    assert!(report.summary_version.is_none());
    assert!(
        report.problems.iter().any(|p| p.contains("usage limit")),
        "the CLI's own explanation must survive: {:?}",
        report.problems
    );

    let row = db.meetings().get(&meeting).unwrap();
    assert_eq!(row.enrich_status.as_deref(), Some("failed"));
    assert!(
        row.enrich_detail
            .as_deref()
            .unwrap()
            .contains("usage limit"),
        "the persisted reason is what the dashboard renders: {:?}",
        row.enrich_detail
    );
}

/// **The path this repo has never asserted.** Engine runs, summary row lands,
/// the UI's source can read it back.
///
/// Every `current_summary` assertion outside `fotw-store` asserts `is_none()`,
/// including both of `tests/summarize.rs`'s deliberate discards — so "a
/// summary was written at all" has been outside the test suite for the whole
/// life of the feature. A regression here is exactly #74.
#[tokio::test]
async fn a_working_engine_writes_a_summary_row_the_ui_can_read() {
    let mut db = db();
    let meeting = meeting_with_transcript(&mut db);
    let cli = working_cli("works");
    cli_settings(&mut db, &cli);

    let report = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &meeting).await;

    assert!(
        report.problems.is_empty(),
        "a clean run reports nothing: {:?}",
        report.problems
    );
    assert!(
        report.summary_version.is_some(),
        "the engine ran but no version came back"
    );

    let summary = db
        .meetings()
        .current_summary(&meeting)
        .unwrap()
        .expect("a summary row must exist — this is the whole feature");
    assert!(
        summary.body_md.contains("interconnect bandwidth"),
        "the engine's prose must reach the stored markdown: {:?}",
        summary.body_md
    );
    assert_eq!(summary.provider, "claude-cli");

    let row = db.meetings().get(&meeting).unwrap();
    assert_eq!(row.enrich_status.as_deref(), Some("ok"));
    assert_eq!(row.enrich_detail, None);
}

/// A CLI that answers both pipeline calls: prose for Call A, a valid (empty)
/// extraction document for Call B. The call it is on is kept on disk rather
/// than in an env var, because the two invocations are two processes.
fn working_cli(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("fotw-enrich-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let envelope = |result: &str| {
        serde_json::json!({
            "type": "result",
            "is_error": false,
            "result": result,
        })
        .to_string()
    };
    std::fs::write(
        dir.join("a.json"),
        envelope("## Notes\n\nThe interconnect bandwidth question is settled.\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("b.json"),
        envelope(
            r#"{"action_items":[],"decisions":[],"open_questions":[],"follow_ups":[],"topics":[]}"#,
        ),
    )
    .unwrap();

    let bin = dir.join("claude");
    std::fs::write(
        &bin,
        "#!/bin/sh\n\
         cat > /dev/null\n\
         d=$(dirname \"$0\")\n\
         if [ -f \"$d/seen\" ]; then cat \"$d/b.json\"; else : > \"$d/seen\"; cat \"$d/a.json\"; fi\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin.to_string_lossy().into_owned()
}

/// A meeting with no transcript at all — recorded with no provider — keeps
/// its fallback title and reports nothing: silence is a normal state.
///
/// Including the report column, which stays NULL. Marking it `no_engine`
/// would be true and useless: it would put a meeting with nothing to
/// summarise into the backfill sweeper's queue for good.
#[tokio::test]
async fn a_meeting_with_no_transcript_is_left_alone() {
    let mut db = db();
    let mut m = NewMeeting::new("dev-1", "UTC");
    m.title = "Untitled recording — 1787372240".to_owned();
    let meeting = db.meetings().create(m).unwrap();

    let report = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &meeting).await;

    let row = db.meetings().get(&meeting).unwrap();
    assert_eq!(row.title, "Untitled recording — 1787372240");
    assert!(report.problems.is_empty());
    assert_eq!(row.enrich_status, None);
    assert_eq!(row.enrich_detail, None);
}
