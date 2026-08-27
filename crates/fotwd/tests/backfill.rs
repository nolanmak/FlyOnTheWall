//! The backfill pass that un-strands meetings that missed enrichment (#74).
//!
//! `insert_summary` has exactly one production caller, so a meeting that
//! misses its single enrichment window — the daemon was restarting, the engine
//! was not installed yet, the binary would not resolve — is unsummarised
//! forever. Thirty-three of them were. `fotwd summarize <id>` existed the
//! whole time and nobody ran it thirty-three times.
//!
//! The pass is deliberately small and deliberately timid: a cap per run, the
//! oldest first, nothing at all without an engine, and never a meeting whose
//! engine already ran and failed.

#![cfg(unix)]

use fotw_secrets::InMemoryKeyStore;
use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotwd::engine::SummarizeSettings;
use fotwd::enrich::backfill_once;
use fotwd::testing::STUB_ENGINE_NAME;

fn db() -> Db {
    Db::open_in_memory(&DbKey::from_bytes([5u8; 32])).unwrap()
}

/// A meeting with a transcript, started at `started`, and no summary.
fn stranded(db: &mut Db, started: i64, status: Option<&str>) -> String {
    let meeting = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC").started_at_ms(started))
        .unwrap();
    let transcript = db
        .meetings()
        .create_transcript(&meeting, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(
            &transcript,
            &[
                NewSegment::new(0, 0, 900, "Um.").channel("system"),
                NewSegment::new(1, 1_000, 4_000, "the interconnect bandwidth question")
                    .channel("system"),
            ],
        )
        .unwrap();
    if let Some(status) = status {
        db.meetings()
            .set_enrich_report(&meeting, status, None)
            .unwrap();
    }
    meeting
}

fn enable_cli(db: &mut Db, binary: &str) {
    let settings = SummarizeSettings {
        cli_enabled: true,
        acknowledged_egress: true,
        binary: binary.to_owned(),
        ..Default::default()
    };
    db.put_setting("summarize", &serde_json::to_string(&settings).unwrap())
        .unwrap();
}

/// A CLI that answers both pipeline calls, for as many meetings as asked.
///
/// The call counter lives on disk because each invocation is a fresh process;
/// even calls are Call A's prose, odd calls are Call B's extraction.
///
/// The stub is [`STUB_ENGINE_NAME`], never `claude`. A configured path that
/// exists is used verbatim, so this was safe by construction — but only while
/// the file is there, and the day it is not, #74's basename rescue would find
/// the developer's real CLI and hand it a fixture transcript (#83).
fn working_cli(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("fotw-backfill-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let envelope = |result: &str| {
        serde_json::json!({"type": "result", "is_error": false, "result": result}).to_string()
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

    let bin = dir.join(STUB_ENGINE_NAME);
    std::fs::write(
        &bin,
        "#!/bin/sh\n\
         cat > /dev/null\n\
         d=$(dirname \"$0\")\n\
         n=$(cat \"$d/n\" 2>/dev/null || echo 0)\n\
         echo $((n + 1)) > \"$d/n\"\n\
         if [ $((n % 2)) -eq 0 ]; then cat \"$d/a.json\"; else cat \"$d/b.json\"; fi\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin.to_string_lossy().into_owned()
}

fn has_summary(db: &mut Db, id: &str) -> bool {
    db.meetings().current_summary(id).unwrap().is_some()
}

/// Oldest first, and the cap holds. A pass that walked the whole library would
/// run 33 CLI invocations back to back on a laptop that is also recording.
#[tokio::test]
async fn the_backfill_takes_the_oldest_stranded_meetings_up_to_the_cap() {
    let mut db = db();
    let oldest = stranded(&mut db, 1_000, Some("no_engine"));
    let middle = stranded(&mut db, 2_000, Some("engine_unresolvable"));
    let newest = stranded(&mut db, 3_000, None);
    enable_cli(&mut db, &working_cli("cap"));

    let pass = backfill_once(&mut db, &InMemoryKeyStore::new(), 2).await;

    assert_eq!(pass.attempted, 2, "the cap must hold");
    assert_eq!(pass.summarised, 2);
    assert_eq!(
        pass.pending, 3,
        "what the pass was looking at when it began"
    );
    assert!(has_summary(&mut db, &oldest));
    assert!(has_summary(&mut db, &middle));
    assert!(
        !has_summary(&mut db, &newest),
        "the newest must wait for the next pass"
    );
    assert_eq!(
        db.meetings().get(&oldest).unwrap().enrich_status.as_deref(),
        Some("ok"),
        "a backfilled meeting stops reporting the state it was stranded in"
    );

    // The next pass picks up exactly where this one left off.
    let pass = backfill_once(&mut db, &InMemoryKeyStore::new(), 2).await;
    assert_eq!(pass.attempted, 1);
    assert_eq!(pass.pending, 1);
    assert!(has_summary(&mut db, &newest));

    // The pass that found nothing. This is the observation #101 is about: it
    // used to be indistinguishable from a task that had died.
    let pass = backfill_once(&mut db, &InMemoryKeyStore::new(), 2).await;
    assert_eq!(pass.attempted, 0);
    assert_eq!(pass.pending, 0);
    assert_eq!(pass.remaining, 0);
    assert!(
        pass.opening_note().contains("0 awaiting"),
        "a pass that ran and found nothing has to say so: {}",
        pass.opening_note()
    );
}

/// A meeting whose engine ran and errored is never retried automatically.
/// Retrying a usage limit once an hour is how a laptop makes a rate limiter's
/// acquaintance; `fotwd summarize <id>` stays the manual retry.
#[tokio::test]
async fn a_failed_meeting_is_never_retried_automatically() {
    let mut db = db();
    let failed = stranded(&mut db, 1_000, Some("failed"));
    enable_cli(&mut db, &working_cli("failed"));

    let pass = backfill_once(&mut db, &InMemoryKeyStore::new(), 5).await;
    assert_eq!(pass.attempted, 0);
    assert_eq!(
        pass.pending, 0,
        "a failed meeting is not awaiting enrichment — it is awaiting a person"
    );
    assert!(!has_summary(&mut db, &failed));
}

/// With no engine, the pass does nothing at all — including not rewriting the
/// report columns. A sweeper that re-stamped `no_engine` every hour would be
/// an hourly write to every stranded meeting for no new information.
#[tokio::test]
async fn with_no_engine_the_backfill_is_a_complete_no_op() {
    let mut db = db();
    let meeting = stranded(&mut db, 1_000, None);

    let pass = backfill_once(&mut db, &InMemoryKeyStore::new(), 5).await;
    assert_eq!(pass.attempted, 0);
    assert_eq!(
        pass.engine, "none",
        "\"no engine\" and \"nothing to do\" are different answers to the \
         question, and the pass has to distinguish them"
    );
    assert_eq!(
        pass.pending, 1,
        "with no engine the pass still has to be able to say how much is waiting"
    );

    let row = db.meetings().get(&meeting).unwrap();
    assert!(!has_summary(&mut db, &meeting));
    assert_eq!(
        row.enrich_status, None,
        "the pass must not have touched the row"
    );
}

/// A meeting that already has a summary is not a candidate, whatever its
/// status column says — the summary is the thing the user sees.
#[tokio::test]
async fn a_meeting_that_already_has_a_summary_is_left_alone() {
    let mut db = db();
    let meeting = stranded(&mut db, 1_000, None);
    enable_cli(&mut db, &working_cli("already"));

    assert_eq!(
        backfill_once(&mut db, &InMemoryKeyStore::new(), 5)
            .await
            .attempted,
        1
    );
    assert!(has_summary(&mut db, &meeting));

    assert_eq!(
        backfill_once(&mut db, &InMemoryKeyStore::new(), 5)
            .await
            .attempted,
        0,
        "a summarised meeting must not be summarised again on the next pass"
    );
}

/// A CLI that refuses both calls, the way a hit usage limit refuses them.
fn failing_cli(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("fotw-backfill-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join(STUB_ENGINE_NAME);
    std::fs::write(&bin, "#!/bin/sh\ncat > /dev/null\nexit 3\n").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin.to_string_lossy().into_owned()
}

/// The backlog a pass reports is **counted at the end, never subtracted**.
///
/// A meeting the engine ran and failed on also leaves the queue —
/// `needing_summary` excludes `failed` so an hourly sweeper cannot retry a
/// usage limit forever — so `pending - summarised` would report a backlog that
/// never drains, on exactly the machine where enrichment is most broken (#101).
#[tokio::test]
async fn the_backlog_a_pass_reports_is_counted_rather_than_subtracted() {
    let mut db = db();
    stranded(&mut db, 1_000, None);
    stranded(&mut db, 2_000, None);
    enable_cli(&mut db, &failing_cli("counted"));

    let pass = backfill_once(&mut db, &InMemoryKeyStore::new(), 1).await;
    assert_eq!(pass.attempted, 1);
    assert_eq!(pass.summarised, 0);
    assert_eq!(pass.failed, 1, "the engine ran and refused");
    assert_eq!(pass.pending, 2, "what was waiting when the pass began");
    assert_eq!(
        pass.remaining, 1,
        "the failed meeting is out of the queue, so one is left — \
         `pending - summarised` would say two forever"
    );
}
