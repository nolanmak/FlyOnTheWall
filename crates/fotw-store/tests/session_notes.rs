mod common;
use fotw_store::{Db, NoteAnchor, NoteDocument, SessionNoteDocument, StoreError};

fn fixture(session: &str) -> SessionNoteDocument {
    SessionNoteDocument {
        session_id: session.into(),
        started_at_ms: 1_000,
        note: NoteDocument {
            revision: 0,
            body_md: "decision café".into(),
            anchors: vec![NoteAnchor::new(0, "decision café", 250)],
        },
    }
}

#[test]
fn recording_notes_survive_reopen_and_stale_tabs_cannot_replace_them() {
    let (_dir, path) = common::tmp_db();
    let key = common::test_key();
    let mut db = Db::open(&path, &key).unwrap();
    let mut draft = fixture("session-A");
    assert_eq!(db.session_note(&draft.session_id).unwrap(), None);
    draft.note.revision = db.save_session_note(&draft, 300).unwrap();
    drop(db);
    let mut reopened = Db::open(&path, &key).unwrap();
    assert_eq!(
        reopened.session_note("session-A").unwrap(),
        Some(draft.clone())
    );
    let stale = fixture("session-A");
    assert!(matches!(
        reopened.save_session_note(&stale, 300),
        Err(StoreError::NoteConflict)
    ));
    assert_eq!(reopened.session_note("session-A").unwrap(), Some(draft));
    assert_eq!(reopened.session_note("session-B").unwrap(), None);
    let bytes = std::fs::read(path).unwrap();
    assert!(!bytes.windows(b"decision".len()).any(|w| w == b"decision"));
}

#[test]
fn draft_bounds_and_original_times_are_checked_without_losing_saved_text() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let mut draft = fixture("session-A");
    assert!(db.save_session_note(&draft, 249).is_err());
    assert_eq!(db.session_note("session-A").unwrap(), None);
    draft.note.revision = db.save_session_note(&draft, 300).unwrap();
    for mutation in 0..4 {
        let mut changed = draft.clone();
        changed.note.body_md = "not saved".into();
        match mutation {
            0 => changed.started_at_ms += 1,
            1 => changed.note.anchors[0].typed_at_ms += 1,
            2 => changed.note.anchors[0].typed_at_ms = -1,
            _ => changed.note.body_md = "x".repeat(fotw_store::MAX_NOTE_BYTES + 1),
        }
        assert!(db.save_session_note(&changed, 500).is_err());
        assert_eq!(db.session_note("session-A").unwrap(), Some(draft.clone()));
    }
    db.conn().execute_batch("CREATE TRIGGER fail_draft BEFORE UPDATE ON session_notes BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let mut changed = draft.clone();
    changed.note.body_md = "failed update".into();
    assert!(db.save_session_note(&changed, 500).is_err());
    assert_eq!(db.session_note("session-A").unwrap(), Some(draft));
}
