//! What the daemon says about itself, from the inside — issue #101.
//!
//! `tests/health.rs` in `fotw-web` pins the wire. This pins the thing behind
//! it: that "has not happened yet" survives all the way out, that a finished
//! pass replaces it, and that the queue depth is read rather than remembered.

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotw_web::DaemonHealth as _;
use fotwd::enrich::BackfillPass;
use fotwd::health::Health;

fn db() -> Db {
    Db::open_in_memory(&DbKey::from_bytes([7u8; 32])).unwrap()
}

/// A meeting with a transcript and no summary: one unit of backlog.
fn stranded(db: &mut Db, started: i64) -> String {
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
            &[NewSegment::new(0, 0, 900, "the ingress cutover").channel("system")],
        )
        .unwrap();
    meeting
}

#[test]
fn a_daemon_that_has_run_nothing_yet_reports_nothing_rather_than_zero() {
    // The whole issue in one assertion. "The backfill has not run since this
    // daemon started" and "the backfill ran and enriched nothing" were the
    // same observation — silence — and three wrong conclusions came out of
    // that. They must not be the same value either.
    let health = Health::new(None, db());
    let report = health.report();

    assert!(report.backfill.is_none());
    assert!(report.github.is_none());
    assert!(report.retention.is_none());
    assert!(
        report.started_at_ms > 1_600_000_000_000,
        "uptime is the first thing asked of a daemon that may have restarted"
    );
}

#[test]
fn a_finished_pass_becomes_the_last_backfill_and_carries_its_engine() {
    let health = Health::new(None, db());
    health.note_backfill(&BackfillPass {
        engine: "codex-cli /opt/homebrew/bin/codex".to_owned(),
        pending: 18,
        attempted: 3,
        summarised: 2,
        failed: 1,
        remaining: 16,
    });

    let report = health.report();
    assert_eq!(report.engine, "codex-cli /opt/homebrew/bin/codex");
    let pass = report.backfill.expect("a pass");
    assert_eq!(
        pass.summary,
        "3 attempted, 2 summarised, 1 not, 16 still awaiting"
    );
    assert!(pass.at_ms > 1_600_000_000_000);
}

#[test]
fn a_pass_that_did_nothing_still_replaces_never_having_run() {
    let health = Health::new(None, db());
    health.note_backfill(&BackfillPass {
        engine: "none".to_owned(),
        ..BackfillPass::default()
    });

    let pass = health.report().backfill.expect("a pass that found nothing");
    assert_eq!(
        pass.summary,
        "0 attempted, 0 summarised, 0 not, 0 still awaiting"
    );
}

#[test]
fn the_queue_depth_is_read_when_asked_rather_than_remembered_from_a_pass() {
    // A number cached at the last pass is up to an hour stale, and the person
    // asking has usually just finished the meeting they are asking about.
    let mut library = db();
    stranded(&mut library, 1_000);
    stranded(&mut library, 2_000);
    let health = Health::new(None, library);

    assert_eq!(health.report().awaiting_enrichment, 2);
}

#[test]
fn the_report_names_the_log_so_the_detail_is_findable() {
    let health = Health::new(Some(std::path::PathBuf::from("/tmp/fotwd.log")), db());
    assert_eq!(
        health.report().log_path.as_deref(),
        Some("/tmp/fotwd.log"),
        "the summary is a pointer to the file that has the rest"
    );
}

#[test]
fn the_pusher_and_the_sweeper_report_the_same_way_the_backfill_does() {
    let health = Health::new(None, db());
    health.note_github("nothing owed");
    health.note_retention("0 evicted, 4.2 GiB on disk");

    let report = health.report();
    assert_eq!(report.github.expect("a round").summary, "nothing owed");
    assert_eq!(
        report.retention.expect("a sweep").summary,
        "0 evicted, 4.2 GiB on disk"
    );
}
