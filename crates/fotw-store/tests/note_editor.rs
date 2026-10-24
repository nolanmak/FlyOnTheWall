use fotw_store::{Db, MAX_NOTE_BYTES, NewMeeting, NoteAnchor};
mod common;

#[test]
fn stale_tab_cannot_overwrite_saved_note_or_anchors() {
    let (_dir, path) = common::tmp_db();
    let mut first = Db::open(&path, &common::test_key()).unwrap();
    let id = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let mut second = Db::open(&path, &common::test_key()).unwrap();
    let a = first.note_document(&id).unwrap();
    let b = second.note_document(&id).unwrap();
    assert_eq!(a.revision, 0);
    let anchors = [NoteAnchor::new(0, "Decision: café", 2300)];
    let revision = first
        .save_note_document(&id, a.revision, "Decision: café", &anchors)
        .unwrap();
    assert!(
        second
            .save_note_document(&id, b.revision, "stale overwrite", &[])
            .is_err()
    );
    let saved = second.note_document(&id).unwrap();
    assert_eq!(saved.revision, revision);
    assert_eq!(saved.body_md, "Decision: café");
    assert_eq!(saved.anchors, anchors);
    assert_eq!(
        second.save_note_document(&id, revision, "", &[]).unwrap(),
        revision + 1
    );
    assert_eq!(first.note_document(&id).unwrap().body_md, "");
}

#[test]
fn invalid_save_preserves_body_anchors_and_revision() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let id = db
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let rev = db
        .save_note_document(&id, 0, "saved", &[NoteAnchor::new(0, "saved", 1200)])
        .unwrap();
    let before = db.note_document(&id).unwrap();
    for (body, anchors) in [
        ("x".repeat(MAX_NOTE_BYTES + 1), vec![]),
        ("replacement".into(), vec![NoteAnchor::new(0, "a", -1)]),
        (
            "replacement".into(),
            vec![NoteAnchor::new(0, "a", 1), NoteAnchor::new(0, "b", 2)],
        ),
    ] {
        assert!(db.save_note_document(&id, rev, &body, &anchors).is_err());
        assert_eq!(db.note_document(&id).unwrap(), before);
    }
    assert!(db.note_document("missing").is_err());
    assert!(db.save_note_document("missing", 0, "text", &[]).is_err());
}

#[test]
fn legacy_writes_invalidate_editor_and_exports_keep_notes() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let id = db
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let first = db.save_note_document(&id, 0, "before", &[]).unwrap();
    db.meetings()
        .upsert_note(&id, "legacy writer", &[])
        .unwrap();
    assert!(
        db.save_note_document(&id, first, "stale editor", &[])
            .is_err()
    );
    let current = db.note_document(&id).unwrap();
    assert_eq!(current.revision, first + 1);
    let anchors = [NoteAnchor::new(1, "Decision", 5000)];
    db.save_note_document(&id, current.revision, "Decision", &anchors)
        .unwrap();
    let exported = db.export_meeting(&id).unwrap();
    assert!(exported.to_markdown().contains("Decision"));
    let json = exported.to_json_pretty();
    assert!(json.contains("5000"));
    assert!(json.contains("Decision"));
}
