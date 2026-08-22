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
/// the machine, and no summary row appears.
#[tokio::test]
async fn without_an_engine_the_fallback_title_lands_and_nothing_egresses() {
    let mut db = db();
    let meeting = meeting_with_transcript(&mut db);

    let report = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &meeting).await;

    let row = db.meetings().get(&meeting).unwrap();
    assert_eq!(row.title, "Okay so the interconnect bandwidth question");
    assert_eq!(report.title.as_deref(), Some(row.title.as_str()));
    assert!(report.summary_version.is_none(), "no engine, no summary");
    assert!(report.problems.is_empty(), "{:?}", report.problems);
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
}

/// A meeting with no transcript at all — recorded with no provider — keeps
/// its fallback title and reports nothing: silence is a normal state.
#[tokio::test]
async fn a_meeting_with_no_transcript_is_left_alone() {
    let mut db = db();
    let mut m = NewMeeting::new("dev-1", "UTC");
    m.title = "Untitled recording — 1787372240".to_owned();
    let meeting = db.meetings().create(m).unwrap();

    let report = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &meeting).await;

    assert_eq!(
        db.meetings().get(&meeting).unwrap().title,
        "Untitled recording — 1787372240"
    );
    assert!(report.problems.is_empty());
}
