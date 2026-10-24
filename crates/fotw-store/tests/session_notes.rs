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

#[test]
fn promotion_preserves_revision_and_routes_late_saves_to_the_meeting() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let mut draft = fixture("session-A");
    draft.note.revision = db.save_session_note(&draft, 300).unwrap();
    draft.note.body_md = "updated before stop".into();
    draft.note.revision = db.save_session_note(&draft, 400).unwrap();
    let meeting = db
        .meetings()
        .create(fotw_store::NewMeeting::new("fixture", "UTC"))
        .unwrap();
    db.promote_session_note("session-A", &meeting, 1000, 500)
        .unwrap();
    assert_eq!(db.note_document(&meeting).unwrap(), draft.note);
    assert_eq!(db.session_note("session-A").unwrap(), Some(draft.clone()));
    draft.note.body_md = "last edit while finishing".into();
    draft.note.revision = db.save_session_note(&draft, 500).unwrap();
    assert_eq!(db.note_document(&meeting).unwrap(), draft.note);
    assert_eq!(
        db.conn()
            .query_row("SELECT COUNT(*) FROM session_notes", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.promote_session_note("session-A", &meeting, 1000, 500)
        .unwrap();
    assert_eq!(db.note_document(&meeting).unwrap(), draft.note);
}

#[test]
fn failed_handoff_preserves_draft_and_creates_neither_note_nor_alias() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let mut draft = fixture("session-A");
    draft.note.revision = db.save_session_note(&draft, 300).unwrap();
    let meeting = db
        .meetings()
        .create(fotw_store::NewMeeting::new("fixture", "UTC"))
        .unwrap();
    db.conn().execute_batch("CREATE TRIGGER fail_link BEFORE INSERT ON session_note_links BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        db.promote_session_note("session-A", &meeting, 1000, 500)
            .is_err()
    );
    assert_eq!(db.session_note("session-A").unwrap(), Some(draft));
    assert_eq!(db.note_document(&meeting).unwrap().revision, 0);
    assert_eq!(db.session_note_meeting("session-A").unwrap(), None);
    assert!(
        db.search(&fotw_store::SearchQuery::new("decision"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn late_save_respects_frozen_bounds_and_conflicts_with_saved_meeting_edits() {
    let (_dir, path) = common::tmp_db();
    let mut live = Db::open(&path, &common::test_key()).unwrap();
    let mut draft = fixture("session-A");
    draft.note.revision = live.save_session_note(&draft, 300).unwrap();
    let meeting = live
        .meetings()
        .create(fotw_store::NewMeeting::new("fixture", "UTC"))
        .unwrap();
    live.promote_session_note("session-A", &meeting, 1000, 500)
        .unwrap();
    let mut saved = Db::open(&path, &common::test_key()).unwrap();
    let mut future = draft.clone();
    future
        .note
        .anchors
        .push(NoteAnchor::new(1, "too late", 501));
    assert!(live.save_session_note(&future, 999_999).is_err());
    saved
        .save_note_document(
            &meeting,
            draft.note.revision,
            "saved editor won",
            &draft.note.anchors,
        )
        .unwrap();
    draft.note.body_md = "late stale edit".into();
    assert!(matches!(
        live.save_session_note(&draft, 500),
        Err(StoreError::NoteConflict)
    ));
    assert_eq!(
        saved.note_document(&meeting).unwrap().body_md,
        "saved editor won"
    );
    assert_eq!(
        live.session_note("session-A")
            .unwrap()
            .unwrap()
            .note
            .body_md,
        "saved editor won"
    );
}

#[test]
fn first_save_after_stop_uses_alias_and_existing_meeting_notes_are_never_replaced() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let meeting = db
        .meetings()
        .create(fotw_store::NewMeeting::new("fixture", "UTC"))
        .unwrap();
    db.promote_session_note("session-A", &meeting, 1000, 500)
        .unwrap();
    assert_eq!(
        db.session_note("session-A").unwrap().unwrap().note.revision,
        0
    );
    let mut draft = fixture("session-A");
    draft.note.revision = db.save_session_note(&draft, 500).unwrap();
    assert_eq!(db.note_document(&meeting).unwrap(), draft.note);
    let other = db
        .meetings()
        .create(fotw_store::NewMeeting::new("fixture", "UTC"))
        .unwrap();
    assert!(
        db.promote_session_note("session-A", &other, 1000, 500)
            .is_err()
    );
    assert!(matches!(
        db.promote_session_note("session-B", &meeting, 1000, 500),
        Err(StoreError::NoteConflict)
    ));
    assert_eq!(db.note_document(&meeting).unwrap(), draft.note);
}
