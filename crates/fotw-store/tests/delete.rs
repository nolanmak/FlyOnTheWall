//! "Delete this meeting" against the shipped, encrypted configuration
//! (docs/REQUIREMENTS.md 9.6).
//!
//! The byte-scan half of §9.6's acceptance criterion is proved against an
//! unencrypted database in `src/delete.rs`, where the assertion is not vacuous
//! — with SQLCipher in the way the phrase is never in the file in plaintext to
//! begin with, so a scan would pass against an implementation that deleted
//! nothing. What is proved *here* is the rest: the cascade, the media unlink,
//! the tombstone, and the fact that the shipped configuration really is
//! encrypted.

use std::path::Path;

use fotw_store::{Db, NewMeeting, NewSegment, NewSummary, NoteAnchor};
use rusqlite::params;

mod common;
use common::{test_key, tmp_db};

const PHRASE: &str = "zarquon-fizzbin-quantum-hedgehog";

#[test]
fn deletion_cleans_retirement_leftovers_and_records_the_original_capture_identity() {
    let (root, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let meeting = db
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let session = "legacy-retirement";
    let pending = root.path().join(format!("sessions/{session}.retired"));
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::write(
        pending.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"claim":{"meeting_id":meeting}})).unwrap(),
    )
    .unwrap();
    std::fs::write(pending.join("raw.pcm"), PHRASE).unwrap();
    db.delete_meeting(&meeting).unwrap();
    assert!(!pending.exists());
    assert!(
        db.finalize_capture(
            session,
            NewMeeting::new("fixture", "UTC").started_at_ms(1_000),
            2_000,
            None
        )
        .is_err()
    );
    // Interrupted cleanup can leave a retired directory without its manifest.
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::write(pending.join("raw.pcm"), PHRASE).unwrap();
    db.delete_meeting(&meeting).unwrap();
    assert!(!pending.exists());
}

#[cfg(unix)]
#[test]
fn deleting_capture_symlink_unlinks_only_the_link() {
    let (root, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let meeting = db
        .finalize_capture(
            "linked-capture",
            NewMeeting::new("fixture", "UTC").started_at_ms(1_000),
            2_000,
            None,
        )
        .unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("keep"), b"unrelated").unwrap();
    std::fs::create_dir_all(root.path().join("sessions")).unwrap();
    let link = root.path().join("sessions/linked-capture");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    db.delete_meeting(&meeting).unwrap();
    assert!(std::fs::symlink_metadata(link).is_err());
    assert!(outside.path().join("keep").exists());
}

#[test]
fn deleting_a_capture_removes_pending_session_audio_using_its_durable_alias() {
    let (root, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let session = "pending-capture-fixture";
    let meeting = db
        .finalize_capture(
            session,
            NewMeeting::new("fixture-device", "UTC").started_at_ms(1_000),
            2_000,
            None,
        )
        .unwrap();
    let pending = root.path().join("sessions").join(session);
    let unrelated = root.path().join("sessions/unrelated-capture");
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::create_dir_all(&unrelated).unwrap();
    std::fs::write(pending.join("system.stt16"), b"synthetic-pending-audio").unwrap();
    std::fs::write(
        pending.join("system.stt-coverage.jsonl"),
        b"synthetic-private-transcript",
    )
    .unwrap();
    // The DB alias must suffice even when a crash left an unreadable manifest.
    std::fs::write(pending.join("manifest.json"), b"{").unwrap();
    std::fs::write(unrelated.join("keep"), b"unrelated").unwrap();
    let outcome = db.delete_meeting(&meeting).unwrap();
    assert!(
        !pending.exists(),
        "deleted meeting left pending capture data behind"
    );
    assert!(outcome.removed_media.contains(&pending));
    assert!(unrelated.join("keep").exists());
}

#[test]
fn a_late_finalize_cannot_resurrect_an_explicitly_deleted_capture() {
    let (root, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let session = "deleted-capture-fixture";
    let meeting = db
        .finalize_capture(
            session,
            NewMeeting::new("fixture-device", "UTC").started_at_ms(1_000),
            2_000,
            None,
        )
        .unwrap();
    db.delete_meeting(&meeting).unwrap();
    drop(db);
    let mut db = Db::open(&path, &test_key()).unwrap();
    assert!(
        db.finalize_capture(
            session,
            NewMeeting::new("fixture-device", "UTC").started_at_ms(1_000),
            2_000,
            None
        )
        .is_err(),
        "a delayed worker recreated a deleted meeting after restart"
    );
    assert!(db.session_note_meeting(session).unwrap().is_none());
    drop(db);
    drop(root);
}

#[test]
fn a_late_note_save_cannot_recreate_a_deleted_capture_draft() {
    let (_root, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let session = "deleted-note-capture";
    let mut draft = fotw_store::SessionNoteDocument {
        session_id: session.into(),
        started_at_ms: 1_000,
        note: fotw_store::NoteDocument {
            revision: 0,
            body_md: PHRASE.into(),
            anchors: vec![],
        },
    };
    draft.note.revision = db.save_session_note(&draft, 500).unwrap();
    let meeting = db
        .finalize_capture(
            session,
            NewMeeting::new("fixture", "UTC").started_at_ms(1_000),
            2_000,
            None,
        )
        .unwrap();
    db.delete_meeting(&meeting).unwrap();
    drop(db);
    let mut db = Db::open(&path, &test_key()).unwrap();
    assert!(db.save_session_note(&draft, 1_000).is_err());
    assert!(db.session_note_drafts(100, 0).unwrap().is_empty());
    assert_eq!(
        db.conn()
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn deletion_retry_finds_capture_data_after_the_alias_has_cascaded_away() {
    let (root, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let session = "retry-deleted-capture";
    let meeting = db
        .finalize_capture(
            session,
            NewMeeting::new("fixture-device", "UTC").started_at_ms(1_000),
            2_000,
            None,
        )
        .unwrap();
    db.delete_meeting(&meeting).unwrap();
    let pending = root.path().join("sessions").join(session);
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::write(pending.join("retained-data"), b"synthetic-cleanup-retry").unwrap();
    assert!(!db.delete_meeting(&meeting).unwrap().existed);
    assert!(!pending.exists());
    assert!(db.meeting_was_deleted(&meeting).unwrap());
}

#[test]
fn legacy_claims_are_deleted_without_requiring_a_note_alias() {
    let (root, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let meeting = db
        .meetings()
        .create(NewMeeting::new("fixture-device", "UTC"))
        .unwrap();
    let pending = root.path().join("sessions/legacy-claimed-capture");
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::write(
        pending.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"claim":{"meeting_id":meeting}})).unwrap(),
    )
    .unwrap();
    std::fs::write(pending.join("system.pcm"), b"synthetic-original").unwrap();
    db.delete_meeting(&meeting).unwrap();
    assert!(!pending.exists());
    assert!(
        db.finalize_capture(
            "legacy-claimed-capture",
            NewMeeting::new("fixture-device", "UTC").started_at_ms(1_000),
            2_000,
            None
        )
        .is_err()
    );
}

#[test]
fn malformed_meeting_identity_cannot_escape_the_media_root() {
    let (root, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    std::fs::create_dir_all(root.path().join("media/2026/08")).unwrap();
    let keep = root.path().join("media/keep");
    std::fs::write(&keep, b"must survive").unwrap();
    assert!(db.delete_meeting("../..").is_err());
    assert!(keep.exists());
}

/// A meeting with a row in every child table, plus its media directory.
fn seed(db: &mut Db, root: &Path) -> (String, String) {
    let meeting_id = db
        .meetings()
        .create(NewMeeting::new("dev-1", "Europe/Berlin").title("Board sync"))
        .unwrap();
    let transcript_id = db
        .meetings()
        .create_transcript(&meeting_id, "deepgram", "nova-3", true)
        .unwrap();

    let segments: Vec<NewSegment> = (0..64)
        .map(|i| NewSegment::new(i, i * 1000, i * 1000 + 900, format!("line {i}: {PHRASE}")))
        .collect();
    db.meetings()
        .append_segments(&transcript_id, &segments)
        .unwrap();

    db.meetings()
        .upsert_note(
            &meeting_id,
            &format!("- {PHRASE}"),
            &[NoteAnchor::new(0, format!("- {PHRASE}"), 12_000)],
        )
        .unwrap();

    db.meetings()
        .insert_summary(
            &meeting_id,
            NewSummary::new("dev-1", "anthropic", "claude", "hash", PHRASE),
        )
        .unwrap();

    let media_rel = format!("media/2026/08/{meeting_id}");
    let media_dir = root.join(&media_rel);
    std::fs::create_dir_all(&media_dir).unwrap();
    // The most commonly forgotten artifact: the provider's raw response holds
    // the full transcript.
    std::fs::write(media_dir.join("raw-deepgram.json.zst"), PHRASE).unwrap();
    std::fs::write(media_dir.join("mic.opus.age"), b"opus bytes").unwrap();

    {
        let c = db.conn();
        c.execute(
            "UPDATE transcripts SET raw_response_rel_path = ?2 WHERE id = ?1",
            params![transcript_id, format!("{media_rel}/raw-deepgram.json.zst")],
        )
        .unwrap();
        c.execute(
            "INSERT INTO recordings (id, meeting_id, channel, rel_path, state, created_at, updated_at, origin_device_id)
             VALUES ('rec-1', ?1, 'mic', ?2, 'complete', 0, 0, 'dev-1')",
            params![meeting_id, format!("{media_rel}/mic.opus.age")],
        )
        .unwrap();
        c.execute(
            "INSERT INTO meeting_participants (id, meeting_id, display_name, created_at, updated_at, origin_device_id)
             VALUES ('p-1', ?1, 'Ada', 0, 0, 'dev-1')",
            params![meeting_id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO action_items (id, meeting_id, text, created_at, updated_at, origin_device_id)
             VALUES ('a-1', ?1, ?2, 0, 0, 'dev-1')",
            params![meeting_id, PHRASE],
        )
        .unwrap();
        c.execute(
            "INSERT INTO tags (id, name, created_at, updated_at, origin_device_id)
             VALUES ('tag-1', 'quarterly', 0, 0, 'dev-1')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO meeting_tags (meeting_id, tag_id, created_at, origin_device_id)
             VALUES (?1, 'tag-1', 0, 'dev-1')",
            params![meeting_id],
        )
        .unwrap();
    }

    (meeting_id, transcript_id)
}

fn count(db: &Db, table: &str, meeting_id: &str) -> i64 {
    db.conn()
        .query_row(
            &format!("SELECT count(*) FROM {table} WHERE meeting_id = ?1"),
            params![meeting_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// One transactional operation, one cascade. Every child table must be empty,
/// including the ones reachable only through a second hop (`segments` via
/// `transcripts`, `note_anchors` via `notes`).
#[test]
fn deleting_a_meeting_cascades_to_every_child_table() {
    let (dir, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let (meeting_id, transcript_id) = seed(&mut db, dir.path());

    for table in [
        "transcripts",
        "segments",
        "note_anchors",
        "notes",
        "summaries",
        "action_items",
        "recordings",
        "meeting_participants",
        "meeting_tags",
    ] {
        assert!(count(&db, table, &meeting_id) > 0, "fixture: {table} empty");
    }

    let outcome = db.delete_meeting(&meeting_id).unwrap();
    assert!(outcome.existed);

    let meetings_left: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM meetings WHERE id = ?1",
            params![meeting_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(meetings_left, 0);
    for table in [
        "transcripts",
        "segments",
        "note_anchors",
        "notes",
        "summaries",
        "action_items",
        "recordings",
        "meeting_participants",
        "meeting_tags",
    ] {
        assert_eq!(
            count(&db, table, &meeting_id),
            0,
            "`{table}` still holds rows for the deleted meeting"
        );
    }

    // The second hop, checked by its own key rather than by meeting_id, so a
    // schema that lost the denormalised `segments.meeting_id` cascade edge
    // cannot pass this by accident.
    let orphan_segments: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM segments WHERE transcript_id = ?1",
            params![transcript_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan_segments, 0);

    // Tags themselves survive; only the link is gone. Deleting a meeting must
    // not delete a label the user also put on forty other meetings.
    let tags: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM tags", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tags, 1);
}

/// The tombstone remembers identity and nothing else.
#[test]
fn deleting_a_meeting_leaves_a_content_free_tombstone() {
    let (dir, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let (meeting_id, _) = seed(&mut db, dir.path());

    db.delete_meeting(&meeting_id).unwrap();

    let (kind, deleted_at, origin): (String, i64, String) = db
        .conn()
        .query_row(
            "SELECT kind, deleted_at, origin_device_id FROM tombstones WHERE id = ?1",
            params![meeting_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "meeting");
    assert_eq!(origin, "dev-1");
    assert!(deleted_at > 0);
}

/// §9.6 names `raw-<provider>.json.zst` as the most commonly forgotten
/// artifact, because it holds the *entire* transcript. The whole
/// `media/<yyyy>/<mm>/<meeting_id>/` directory goes, and this scans every
/// remaining file under the root for the phrase.
#[test]
fn deleting_a_meeting_unlinks_its_media_directory_including_the_raw_response() {
    let (dir, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let (meeting_id, _) = seed(&mut db, dir.path());

    let media_dir = dir.path().join(format!("media/2026/08/{meeting_id}"));
    assert!(media_dir.join("raw-deepgram.json.zst").exists());

    let outcome = db.delete_meeting(&meeting_id).unwrap();
    assert!(!media_dir.exists(), "the media directory survived");
    assert!(
        outcome.removed_media.iter().any(|p| p == &media_dir),
        "the outcome must name what it removed: {:?}",
        outcome.removed_media
    );

    for file in walk(dir.path()) {
        let bytes = std::fs::read(&file).unwrap_or_default();
        assert!(
            !contains(&bytes, PHRASE.as_bytes()),
            "transcript text survives in {}",
            file.display()
        );
    }
}

/// The shipped configuration, byte-scanned. This does not prove the delete
/// worked — see the module docs — it proves the database is encrypted, which
/// is the other half of why nothing is readable.
#[test]
fn the_encrypted_library_never_holds_the_phrase_in_plaintext() {
    let (dir, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let (meeting_id, _) = seed(&mut db, dir.path());
    db.delete_meeting(&meeting_id).unwrap();
    drop(db);

    for suffix in ["", "-wal", "-shm"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        let bytes = std::fs::read(&p).unwrap_or_default();
        assert!(
            !contains(&bytes, PHRASE.as_bytes()),
            "plaintext found in {}",
            p.display()
        );
    }
}

/// Deleting twice, or deleting something that never existed, must not throw:
/// the UI retries this after a crash, and a tombstone for an unknown id is
/// still the correct record of intent.
#[test]
fn deleting_is_idempotent() {
    let (dir, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let (meeting_id, _) = seed(&mut db, dir.path());

    assert!(db.delete_meeting(&meeting_id).unwrap().existed);
    assert!(!db.delete_meeting(&meeting_id).unwrap().existed);
    assert!(!db.delete_meeting("never-existed").unwrap().existed);

    let n: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM tombstones", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "one tombstone per id, not one per attempt");
}

/// §9.7 invariant 5 is a schema rule, so a row that breaks it is a corrupt
/// row, not an instruction. `remove_dir_all` on an attacker-controlled
/// absolute path is the worst possible way to find that out.
#[test]
fn a_stored_absolute_path_is_refused_rather_than_followed() {
    let (dir, path) = tmp_db();
    let mut db = Db::open(&path, &test_key()).unwrap();
    let (meeting_id, _) = seed(&mut db, dir.path());

    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, b"must survive").unwrap();

    db.conn()
        .execute(
            "UPDATE recordings SET rel_path = ?1 WHERE meeting_id = ?2",
            params![outside.to_string_lossy(), meeting_id],
        )
        .unwrap();

    let outcome = db.delete_meeting(&meeting_id).unwrap();
    assert!(outside.exists(), "an absolute path in the DB was followed");
    assert!(
        outcome
            .missing_media
            .iter()
            .any(|p| p == &outside.to_string_lossy()),
        "a rejected path must be reported, not swallowed: {outcome:?}"
    );
}

/// Belt and braces on the bootstrap: if `PRAGMA foreign_keys` ever regressed,
/// the cascade test above would still pass on a fresh database (nothing to
/// orphan) but this would fail immediately.
#[test]
fn foreign_keys_reject_an_orphan_segment() {
    let (_dir, path) = tmp_db();
    let db = Db::open(&path, &test_key()).unwrap();
    let err = db
        .conn()
        .execute(
            "INSERT INTO segments (id, transcript_id, meeting_id, idx, start_ms, end_ms, channel, text)
             VALUES ('s1', 'nope', 'nope', 0, 0, 1, 'mic', 'hi')",
            [],
        )
        .unwrap_err();
    assert!(
        err.to_string().to_uppercase().contains("FOREIGN KEY"),
        "expected an FK violation, got: {err}"
    );
}

fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
