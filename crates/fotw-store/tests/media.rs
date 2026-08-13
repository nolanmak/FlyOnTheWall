//! The `recordings` table and the inventory the §9.5 sweeper reads.
//!
//! `recordings` has been in migration 0001 since the beginning with nothing
//! writing to it. These are the tests for the code that finally does — and for
//! the one query whose shape decides whether the sweeper can be trusted with
//! irreversible deletion.

use fotw_store::{Db, NewMeeting, NewRecording, NewSegment};

mod common;
use common::test_key;

fn db() -> Db {
    Db::open_in_memory(&test_key()).unwrap()
}

fn meeting(db: &mut Db, started_at_ms: i64) -> String {
    let mut m = NewMeeting::new("dev-1", "UTC");
    m.started_at_ms = started_at_ms;
    db.meetings().create(m).unwrap()
}

fn track(channel: &str, rel_path: &str, bytes: u64) -> NewRecording {
    NewRecording {
        channel: channel.to_owned(),
        rel_path: rel_path.to_owned(),
        bytes,
        duration_ms: 2_000,
        sample_rate_hz: 16_000,
    }
}

fn segment(idx: i64, text: &str) -> NewSegment {
    NewSegment {
        idx,
        start_ms: idx * 1_000,
        end_ms: idx * 1_000 + 900,
        channel: "system".to_owned(),
        speaker_label: None,
        person_id: None,
        text: text.to_owned(),
        confidence: Some(0.9),
        is_final: true,
        words: None,
    }
}

#[test]
fn a_promoted_track_is_stored_once_per_channel_and_a_re_promotion_updates_it_in_place() {
    // Promotion is idempotent by design (a crash mid-way is finished on the
    // next run), so the row it writes has to be too. `UNIQUE (meeting_id,
    // channel)` would otherwise turn every resumed promotion into an error.
    let mut db = db();
    let id = meeting(&mut db, 1_786_579_200_000);

    let a = db
        .upsert_recording(&id, &track("system", "media/2026/08/x/system.opus", 4_096))
        .unwrap();
    let b = db
        .upsert_recording(&id, &track("system", "media/2026/08/x/system.opus", 8_192))
        .unwrap();
    assert_eq!(a, b, "the second promotion minted a second row");

    let rows = db.audio_inventory().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].audio.len(), 1);
    assert_eq!(
        rows[0].audio[0].bytes,
        Some(8_192),
        "the re-promotion did not correct the size"
    );

    db.upsert_recording(&id, &track("mic", "media/2026/08/x/mic.opus", 2_048))
        .unwrap();
    let rows = db.audio_inventory().unwrap();
    assert_eq!(rows[0].audio.len(), 2, "the two legs are separate rows");
}

#[test]
fn an_absolute_or_escaping_path_is_refused_because_the_library_has_to_be_movable() {
    // §9.7 invariant 5. A stored absolute path breaks restore-onto-another-
    // machine; a `..` turns the sweeper's `remove_file` into a way out of the
    // data root. Both are rejected at the write, which is the only place the
    // check is cheap.
    let mut db = db();
    let id = meeting(&mut db, 0);

    for bad in [
        "/Users/someone/audio.opus",
        "../../etc/passwd",
        "media/../../escape.opus",
    ] {
        assert!(
            db.upsert_recording(&id, &track("system", bad, 1)).is_err(),
            "`{bad}` was accepted into recordings.rel_path"
        );
    }
    assert!(
        db.upsert_recording(&id, &track("system", "media/2026/08/x/system.opus", 1))
            .is_ok()
    );
}

/// The query the whole sweeper rests on.
///
/// `meetings.state` says `ready` for a session that finished with no provider
/// configured, so it cannot be the answer to "does a transcript exist". Nor
/// can "a transcripts row exists": `create_transcript` is called before the
/// segments are appended, so between those two statements — and forever after,
/// if the append failed — the row is there and the text is not.
#[test]
fn the_inventory_reports_a_transcript_ready_time_only_when_segments_actually_exist() {
    let mut db = db();

    // 1. Ready, with no transcript row at all: recorded without a provider.
    let no_provider = meeting(&mut db, 1_000);
    db.meetings().set_state(&no_provider, "ready").unwrap();

    // 2. Ready, with a transcript row that holds no segments.
    let empty = meeting(&mut db, 2_000);
    db.meetings()
        .create_transcript(&empty, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings().set_state(&empty, "ready").unwrap();

    // 3. Ready, with a transcript that has text.
    let real = meeting(&mut db, 3_000);
    let t = db
        .meetings()
        .create_transcript(&real, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(&t, &[segment(0, "we should ship it")])
        .unwrap();
    db.meetings().set_state(&real, "ready").unwrap();

    let rows = db.audio_inventory().unwrap();
    let find = |id: &str| {
        rows.iter()
            .find(|r| r.meeting_id == id)
            .unwrap_or_else(|| panic!("{id} missing from the inventory"))
    };

    assert_eq!(find(&no_provider).state, "ready");
    assert!(
        find(&no_provider).transcript_ready_at_ms.is_none(),
        "a meeting with no transcript reported one; its audio would be evictable"
    );
    assert!(
        find(&empty).transcript_ready_at_ms.is_none(),
        "an empty transcript row reported a transcript"
    );
    assert!(
        find(&real).transcript_ready_at_ms.is_some(),
        "a real transcript was not reported, so its audio would never be swept"
    );
    assert!(
        find(&real).transcript_bytes > 0 && find(&no_provider).transcript_bytes == 0,
        "the text accounting does not match the transcripts"
    );
}

#[test]
fn the_inventory_carries_the_retention_policy_columns_and_the_audio_paths() {
    let mut db = db();
    let id = meeting(&mut db, 5_000);
    db.upsert_recording(&id, &track("system", "media/2026/08/y/system.opus", 10))
        .unwrap();
    db.upsert_recording(&id, &track("mic", "media/2026/08/y/mic.opus", 20))
        .unwrap();
    db.set_retain_audio(&id, "days", Some(90)).unwrap();

    let rows = db.audio_inventory().unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.retain_audio, "days");
    assert_eq!(row.retain_audio_days, Some(90));
    assert_eq!(row.started_at_ms, 5_000);

    let mut paths: Vec<&str> = row.audio.iter().map(|a| a.rel_path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        ["media/2026/08/y/mic.opus", "media/2026/08/y/system.opus"]
    );
}

#[test]
fn marking_audio_deleted_keeps_the_row_so_the_library_can_say_the_audio_is_gone() {
    // The schema comment on `recordings.deleted_at` is explicit about this:
    // the row survives the bytes so the UI can say "audio was deleted on
    // <date>" rather than pretending the meeting never had any. Dropping the
    // row would also make the sweeper's own accounting unable to distinguish
    // "never recorded" from "reclaimed".
    let mut db = db();
    let id = meeting(&mut db, 9_000);
    db.upsert_recording(&id, &track("system", "media/2026/08/z/system.opus", 4_096))
        .unwrap();

    let n = db.mark_audio_deleted(&id, 1_800_000_000_000).unwrap();
    assert_eq!(n, 1);

    let rows = db.audio_inventory().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].audio.is_empty(),
        "deleted audio still counts toward the disk budget"
    );

    let (state, deleted_at, rel): (String, Option<i64>, String) = db
        .conn()
        .query_row(
            "SELECT state, deleted_at, rel_path FROM recordings WHERE meeting_id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "deleted");
    assert_eq!(deleted_at, Some(1_800_000_000_000));
    assert_eq!(
        rel, "media/2026/08/z/system.opus",
        "the path is kept; it is not content, and it is how a restore finds it"
    );
}

#[test]
fn the_purge_deadline_is_written_back_so_the_ui_can_show_when_audio_goes() {
    let mut db = db();
    let id = meeting(&mut db, 9_000);
    db.upsert_recording(&id, &track("system", "media/2026/08/w/system.opus", 1))
        .unwrap();

    db.set_purge_after(&id, Some(1_800_000_000_000)).unwrap();
    let rows = db.audio_inventory().unwrap();
    assert_eq!(rows[0].purge_after_ms, Some(1_800_000_000_000));

    // A policy change to `forever` clears it rather than leaving a stale date
    // on screen.
    db.set_purge_after(&id, None).unwrap();
    assert_eq!(db.audio_inventory().unwrap()[0].purge_after_ms, None);
}

#[test]
fn retention_settings_round_trip_through_the_settings_table() {
    let mut db = db();
    assert_eq!(db.get_setting("retention").unwrap(), None);

    db.put_setting("retention", r#"{"default_days":7,"budget_bytes":123}"#)
        .unwrap();
    assert_eq!(
        db.get_setting("retention").unwrap().as_deref(),
        Some(r#"{"default_days":7,"budget_bytes":123}"#)
    );

    // Upsert, not insert: a second write is a change of mind, not a
    // constraint violation.
    db.put_setting("retention", r#"{"default_days":30,"budget_bytes":456}"#)
        .unwrap();
    assert_eq!(
        db.get_setting("retention").unwrap().as_deref(),
        Some(r#"{"default_days":30,"budget_bytes":456}"#)
    );
}

#[test]
fn deleting_a_meeting_takes_its_recording_rows_with_it() {
    // The §9.6 cascade, from this table's point of view. A surviving row would
    // leave the inventory pointing at a meeting that no longer exists, and the
    // sweeper counting bytes for it forever.
    let mut db = db();
    let id = meeting(&mut db, 1);
    db.upsert_recording(&id, &track("system", "media/2026/08/q/system.opus", 1))
        .unwrap();
    assert_eq!(db.audio_inventory().unwrap().len(), 1);

    db.delete_meeting(&id).unwrap();
    assert!(db.audio_inventory().unwrap().is_empty());
    let n: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM recordings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}
