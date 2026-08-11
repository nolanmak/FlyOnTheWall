//! `MeetingRepo` — the typed handlers, and the invariants they exist to keep
//! (docs/REQUIREMENTS.md 9.1, 9.3).

use fotw_store::{Db, NewMeeting, NewSegment, NewSummary, NoteAnchor, StoreError};

mod common;
use common::test_key;

fn db() -> Db {
    Db::open_in_memory(&test_key()).unwrap()
}

#[test]
fn create_then_get_round_trips_a_meeting() {
    let mut db = db();
    let id = db
        .meetings()
        .create(
            NewMeeting::new("dev-1", "Europe/Berlin")
                .title("Weekly sync")
                .started_at_ms(1_700_000_000_000)
                .disclosed(true),
        )
        .unwrap();

    let m = db.meetings().get(&id).unwrap();
    assert_eq!(m.id, id);
    assert_eq!(m.title, "Weekly sync");
    assert_eq!(m.tz, "Europe/Berlin");
    assert_eq!(m.started_at_ms, 1_700_000_000_000);
    assert_eq!(m.state, "recording", "a new meeting starts recording");
    assert!(m.disclosed, "consent is a first-class field, not a UI flag");
    assert_eq!(
        m.retain_audio, "default",
        "retention falls back to the global policy"
    );
    assert_eq!(m.lamport, 0);
    assert_eq!(m.origin_device_id, "dev-1");
    assert_eq!(m.created_at, m.updated_at);
    assert_eq!(m.ended_at_ms, None);
    assert_eq!(m.duration_ms, None);

    // UUIDv7, not something else that happens to be unique.
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "7");
}

#[test]
fn getting_a_missing_meeting_is_not_found_rather_than_an_empty_row() {
    let mut db = db();
    let err = db.meetings().get("nope").unwrap_err();
    assert!(matches!(
        err,
        StoreError::NotFound {
            kind: "meeting",
            ..
        }
    ));
}

/// The home screen's query. Newest first, and paginated — a library with ten
/// thousand meetings must not be loaded to show twenty.
#[test]
fn list_returns_the_most_recent_meetings_first() {
    let mut db = db();
    for i in 0..5i64 {
        db.meetings()
            .create(
                NewMeeting::new("dev-1", "UTC")
                    .title(format!("m{i}"))
                    .started_at_ms(1_000 + i * 1_000),
            )
            .unwrap();
    }

    let page = db.meetings().list(3, 0).unwrap();
    let titles: Vec<_> = page.iter().map(|m| m.title.as_str()).collect();
    assert_eq!(titles, ["m4", "m3", "m2"]);

    let page = db.meetings().list(3, 3).unwrap();
    let titles: Vec<_> = page.iter().map(|m| m.title.as_str()).collect();
    assert_eq!(titles, ["m1", "m0"]);
}

/// Every mutation bumps `lamport`, because merge order is
/// `(lamport, origin_device_id)` and a mutation that forgot to bump it is a
/// mutation a future sync would silently discard.
#[test]
fn state_changes_bump_the_merge_counter() {
    let mut db = db();
    let id = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC"))
        .unwrap();

    db.meetings().set_state(&id, "transcribing").unwrap();
    assert_eq!(db.meetings().get(&id).unwrap().lamport, 1);

    db.meetings().set_state(&id, "ready").unwrap();
    let m = db.meetings().get(&id).unwrap();
    assert_eq!(m.state, "ready");
    assert_eq!(m.lamport, 2);

    db.meetings()
        .finish(&id, m.started_at_ms + 45 * 60_000)
        .unwrap();
    let m = db.meetings().get(&id).unwrap();
    assert_eq!(m.duration_ms, Some(45 * 60_000));
    assert_eq!(m.lamport, 3);

    assert!(matches!(
        db.meetings().set_state("nope", "ready").unwrap_err(),
        StoreError::NotFound { .. }
    ));
}

/// Re-transcribing with a different provider must not destroy the old
/// transcript, and promoting the new one must be atomic — the partial unique
/// index makes a two-statement version fail rather than corrupt.
#[test]
fn a_meeting_keeps_every_transcript_and_exactly_one_is_primary() {
    let mut db = db();
    let meeting = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC"))
        .unwrap();

    let deepgram = db
        .meetings()
        .create_transcript(&meeting, "deepgram", "nova-3", true)
        .unwrap();
    let eleven = db
        .meetings()
        .create_transcript(&meeting, "elevenlabs", "scribe-v1", true)
        .unwrap();

    let both: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM transcripts WHERE meeting_id = ?1",
            [&meeting],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(both, 2, "the old transcript must survive re-transcription");

    let primary: String = db
        .conn()
        .query_row(
            "SELECT id FROM transcripts WHERE meeting_id = ?1 AND is_primary = 1",
            [&meeting],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(primary, eleven);

    // And back again.
    db.meetings().set_primary_transcript(&deepgram).unwrap();
    let primaries: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM transcripts WHERE meeting_id = ?1 AND is_primary = 1",
            [&meeting],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(primaries, 1);
    let primary: String = db
        .conn()
        .query_row(
            "SELECT id FROM transcripts WHERE meeting_id = ?1 AND is_primary = 1",
            [&meeting],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(primary, deepgram);
}

/// Segments arrive in bursts and land in one transaction. `meeting_id` is
/// copied from the transcript rather than taken from the caller, so the
/// denormalised column cannot drift from the join it replaces.
#[test]
fn appended_segments_carry_word_blobs_and_inherit_their_meeting() {
    let mut db = db();
    let meeting = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC"))
        .unwrap();
    let transcript = db
        .meetings()
        .create_transcript(&meeting, "deepgram", "nova-3", true)
        .unwrap();

    let words = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x01, 0x02];
    db.meetings()
        .append_segments(
            &transcript,
            &[
                NewSegment::new(0, 0, 1_000, "hello there").words(words.clone()),
                NewSegment::new(1, 1_000, 2_400, "general kenobi").channel("system"),
            ],
        )
        .unwrap();

    let text = db.meetings().transcript_text(&transcript).unwrap();
    assert_eq!(
        text,
        vec![
            (0, "hello there".to_owned()),
            (1, "general kenobi".to_owned())
        ]
    );

    let (owner, blob, channel): (String, Option<Vec<u8>>, String) = db
        .conn()
        .query_row(
            "SELECT meeting_id, words, channel FROM segments WHERE transcript_id = ?1 AND idx = 0",
            [&transcript],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(owner, meeting);
    assert_eq!(blob, Some(words), "word timings round-trip as a BLOB");
    assert_eq!(channel, "mic");

    // Empty batches are a no-op, not an error: the STT layer flushes on a
    // timer and most ticks have nothing to write.
    db.meetings().append_segments(&transcript, &[]).unwrap();

    assert!(matches!(
        db.meetings()
            .append_segments("no-such-transcript", &[NewSegment::new(0, 0, 1, "x")])
            .unwrap_err(),
        StoreError::NotFound {
            kind: "transcript",
            ..
        }
    ));
}

/// A block's `typed_at_ms` is only meaningful together with the `block_text`
/// it was captured against, so anchors are replaced wholesale with the body in
/// one transaction. A half-updated anchor set would point the augmentation
/// prompt at the wrong span of transcript and produce a confidently wrong
/// expansion.
#[test]
fn upserting_a_note_replaces_its_anchors_wholesale() {
    let mut db = db();
    let meeting = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC"))
        .unwrap();

    let note_id = db
        .meetings()
        .upsert_note(
            &meeting,
            "- ship it\n- ask Ada",
            &[
                NoteAnchor::new(0, "- ship it", 12_000),
                NoteAnchor::new(1, "- ask Ada", 47_500),
            ],
        )
        .unwrap();

    let (body, anchors) = db.meetings().note(&meeting).unwrap().unwrap();
    assert_eq!(body, "- ship it\n- ask Ada");
    assert_eq!(anchors.len(), 2);
    assert_eq!(anchors[1].typed_at_ms, 47_500);
    assert_eq!(anchors[1].block_text, "- ask Ada");

    // Editing the document down to one block leaves one anchor, not three.
    let same_id = db
        .meetings()
        .upsert_note(
            &meeting,
            "- ship it today",
            &[NoteAnchor::new(0, "- ship it today", 12_000)],
        )
        .unwrap();
    assert_eq!(same_id, note_id, "one live note document per meeting");

    let (body, anchors) = db.meetings().note(&meeting).unwrap().unwrap();
    assert_eq!(body, "- ship it today");
    assert_eq!(anchors.len(), 1);

    let lamport: i64 = db
        .conn()
        .query_row("SELECT lamport FROM notes WHERE id = ?1", [&note_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(lamport, 1);

    assert!(db.meetings().note("no-such-meeting").unwrap().is_none());
}

/// The acceptance criterion for the append-only summary design: inserting v2
/// leaves exactly one `is_current` row, and v1 is still there to go back to.
#[test]
fn inserting_a_second_summary_version_leaves_exactly_one_current() {
    let mut db = db();
    let meeting = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC"))
        .unwrap();

    let v1 = db
        .meetings()
        .insert_summary(
            &meeting,
            NewSummary::new("dev-1", "anthropic", "claude-opus-5", "sha-1", "# First")
                .coverage(0.82),
        )
        .unwrap();
    assert_eq!(v1.version, 1);
    assert!(v1.is_current);
    assert_eq!(v1.coverage, Some(0.82));

    let v2 = db
        .meetings()
        .insert_summary(
            &meeting,
            NewSummary::new("dev-1", "anthropic", "claude-opus-5", "sha-2", "# Second"),
        )
        .unwrap();
    assert_eq!(v2.version, 2);
    assert!(v2.is_current);

    let current_rows: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM summaries WHERE meeting_id = ?1 AND is_current = 1",
            [&meeting],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(current_rows, 1, "exactly one summary may be current");

    let current = db.meetings().current_summary(&meeting).unwrap().unwrap();
    assert_eq!(current.id, v2.id);
    assert_eq!(current.body_md, "# Second");

    // v1 is still there. Regenerating must never destroy the previous answer.
    let history = db.meetings().summary_versions(&meeting).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].version, 2);
    assert_eq!(history[1].version, 1);
    assert_eq!(history[1].body_md, "# First");
    assert!(!history[1].is_current);
    assert_eq!(history[1].prompt_hash, "sha-1");
}

#[test]
fn a_summary_for_an_unknown_meeting_is_rejected_by_name() {
    let mut db = db();
    let err = db
        .meetings()
        .insert_summary(
            "nope",
            NewSummary::new("dev-1", "anthropic", "claude", "sha", "body"),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::NotFound {
            kind: "meeting",
            ..
        }
    ));
    assert!(err.to_string().contains("no such meeting"));
}

#[test]
fn a_meeting_with_no_summary_has_none() {
    let mut db = db();
    let meeting = db
        .meetings()
        .create(NewMeeting::new("dev-1", "UTC"))
        .unwrap();
    assert!(db.meetings().current_summary(&meeting).unwrap().is_none());
    assert!(db.meetings().summary_versions(&meeting).unwrap().is_empty());
}
