use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotwd::persist::dedupe_meeting;

const ECHO: &str = "We will ship the thistledown build on Friday";
const FOLLOW_UP: &str = "The next review is scheduled for Monday";

fn seed_meeting(db: &mut Db) -> (String, String) {
    let mut repo = db.meetings();
    let meeting = repo.create(NewMeeting::new("test-device", "UTC")).unwrap();
    let original = repo
        .create_transcript(&meeting, "deepgram", "nova-3", true)
        .unwrap();
    repo.append_segments(
        &original,
        &[
            NewSegment::new(0, 0, 3_000, ECHO).channel("system"),
            NewSegment::new(1, 500, 3_500, ECHO),
            NewSegment::new(2, 5_000, 8_000, FOLLOW_UP).channel("system"),
        ],
    )
    .unwrap();
    (meeting, original)
}

fn fail_second_segment_insert(db: &mut Db, meeting: &str) {
    db.conn()
        .execute_batch(
            "CREATE TEMP TRIGGER reject_dedupe_segment BEFORE INSERT ON main.segments
             WHEN NEW.idx = 1
             BEGIN
                 SELECT RAISE(ABORT, 'synthetic dedupe segment rejection');
             END",
        )
        .unwrap();
    let err = dedupe_meeting(db, meeting).unwrap_err();
    assert!(
        err.to_string()
            .contains("synthetic dedupe segment rejection"),
        "{err}"
    );
}

#[test]
fn dedupe_segment_failure_preserves_the_original_primary() {
    let mut db = Db::open_in_memory(&DbKey::from_bytes([0x45; 32])).unwrap();
    let (meeting, original) = seed_meeting(&mut db);
    let before = db.export_meeting(&meeting).unwrap();

    fail_second_segment_insert(&mut db, &meeting);

    assert_eq!(
        db.meetings().primary_transcript_id(&meeting).unwrap(),
        Some(original),
        "the original transcript must still be primary after a failed segment insert"
    );
    let after = db.export_meeting(&meeting).unwrap();
    assert_eq!(after.transcripts, before.transcripts);
    assert_eq!(after.segments, before.segments);
}

#[test]
fn dedupe_retries_after_a_segment_insert_failure() {
    let mut db = Db::open_in_memory(&DbKey::from_bytes([0x45; 32])).unwrap();
    let (meeting, _) = seed_meeting(&mut db);
    fail_second_segment_insert(&mut db, &meeting);
    db.conn()
        .execute_batch("DROP TRIGGER reject_dedupe_segment")
        .unwrap();

    let report = dedupe_meeting(&mut db, &meeting).unwrap();
    assert_eq!(report.dropped, 1, "retry must still find the mic echo");
    assert_eq!(report.before, 3);
    let new = report.new_transcript.unwrap();
    let repo = db.meetings();
    assert_eq!(
        repo.primary_transcript_id(&meeting).unwrap(),
        Some(new.clone())
    );
    assert_eq!(repo.transcript_text(&new).unwrap().len(), 2);
}

#[test]
fn dedupe_publishes_a_complete_version_and_can_revert() {
    let mut db = Db::open_in_memory(&DbKey::from_bytes([0x45; 32])).unwrap();
    let (meeting, original) = seed_meeting(&mut db);
    let original_segments = db.meetings().transcript_segments(&original).unwrap();

    let report = dedupe_meeting(&mut db, &meeting).unwrap();
    assert_eq!((report.before, report.dropped), (3, 1));
    let new = report.new_transcript.unwrap();
    let export = db.export_meeting(&meeting).unwrap();
    assert_eq!(export.transcripts.len(), 2);
    let old_row = export
        .transcripts
        .iter()
        .find(|row| row.id == original)
        .unwrap();
    let new_row = export.transcripts.iter().find(|row| row.id == new).unwrap();
    assert_eq!(old_row.is_primary, 0);
    assert_eq!(new_row.is_primary, 1);
    assert_eq!(new_row.provider, old_row.provider);
    assert_eq!(new_row.model, old_row.model);
    let mut repo = db.meetings();
    let mut expected = vec![original_segments[0].clone(), original_segments[2].clone()];
    expected[1].idx = 1;
    assert_eq!(repo.transcript_segments(&new).unwrap(), expected);

    // This is the same store operation used by `fotwd dedupe --revert`.
    repo.set_primary_transcript(&original).unwrap();
    assert_eq!(
        repo.primary_transcript_id(&meeting).unwrap().as_deref(),
        Some(original.as_str())
    );
    assert_eq!(
        repo.transcript_segments(&original).unwrap(),
        original_segments
    );
}

#[test]
fn dedupe_reader_sees_the_original_until_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite3");
    let key = DbKey::from_bytes([0x45; 32]);
    let mut writer = Db::open(&path, &key).unwrap();
    let (meeting, original) = seed_meeting(&mut writer);
    let reader = Db::open(&path, &key).unwrap();
    let (send, receive) = std::sync::mpsc::channel();

    // The hook runs inside each segment insert, before the writer commits.
    // Read the primary and its segment count in one snapshot.
    let read_meeting = meeting.clone();
    writer
        .conn()
        .update_hook(Some(move |_, _: &str, table: &str, _| {
            if table == "segments" {
                let snapshot = reader.conn().query_row(
                    "SELECT t.id, COUNT(s.id)
                     FROM transcripts t LEFT JOIN segments s ON s.transcript_id = t.id
                     WHERE t.meeting_id = ?1 AND t.is_primary = 1
                     GROUP BY t.id",
                    [&read_meeting],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                );
                let _ = send.send(snapshot);
            }
        }))
        .unwrap();

    let report = dedupe_meeting(&mut writer, &meeting).unwrap();
    let new = report.new_transcript.unwrap();
    let snapshots: Vec<_> = receive.try_iter().map(Result::unwrap).collect();
    assert_eq!(snapshots, vec![(original, 3); 2]);

    let repo = writer.meetings();
    assert_eq!(
        repo.primary_transcript_id(&meeting).unwrap(),
        Some(new.clone())
    );
    assert_eq!(repo.transcript_text(&new).unwrap().len(), 2);
}
