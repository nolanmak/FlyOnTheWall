use fotw_store::{Db, DbKey, NewMeeting, NewSummary};

fn summary(body: &str) -> NewSummary {
    NewSummary {
        transcript_id: None,
        provider: "fixture".into(),
        model: "fixture".into(),
        prompt_hash: "fixture".into(),
        body_md: body.into(),
        coverage: None,
        input_tokens: None,
        output_tokens: None,
        cost_micros: None,
        template_id: None,
        origin_device_id: "fixture".into(),
    }
}

#[test]
fn a_changed_setting_cannot_publish_any_generated_artifact_or_demote_old_results() {
    let mut db = Db::open_in_memory(&DbKey::from_bytes([7; 32])).unwrap();
    let id = db
        .meetings()
        .create(NewMeeting::new("fixture", "UTC").title("Original title"))
        .unwrap();
    let old = db
        .meetings()
        .insert_summary(&id, summary("Original summary"))
        .unwrap();
    db.save_sharing_document(&id, 0, "original document")
        .unwrap();
    db.put_setting("summarize", "hosted").unwrap();
    let version = db.setting_version("summarize").unwrap();
    db.put_setting("summarize", "off").unwrap();
    db.put_setting("summarize", "hosted").unwrap();
    assert!(
        !version.is_current(&db).unwrap(),
        "value equality cannot revive an old generation"
    );
    let refused = [
        db.meetings()
            .set_title_if_setting(&id, "Stale title", &version)
            .is_err(),
        db.meetings()
            .insert_summary_if_setting(&id, summary("Stale summary"), &version)
            .is_err(),
        db.save_sharing_document_if_setting(&id, 1, "stale document", &version)
            .is_err(),
    ];
    assert_eq!(
        refused, [true; 3],
        "title, summary and document must all be fenced"
    );
    assert_eq!(db.meetings().get(&id).unwrap().title, "Original title");
    assert_eq!(
        db.meetings().current_summary(&id).unwrap().unwrap().id,
        old.id
    );
    assert_eq!(
        db.sharing_document(&id).unwrap(),
        Some((1, "original document".into()))
    );
}

#[test]
fn unchanged_settings_allow_generated_writes_and_unrelated_settings_do_not_cancel_them() {
    let mut db = Db::open_in_memory(&DbKey::from_bytes([7; 32])).unwrap();
    let id = db
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    // Exercise a genuinely absent generic setting, rather than the new
    // library default inserted by bootstrap.
    db.conn()
        .execute("DELETE FROM settings WHERE key='summarize'", [])
        .unwrap();
    let missing = db.setting_version("summarize").unwrap();
    assert_eq!(missing.value(), None);
    db.put_setting("unrelated", "changed").unwrap();
    assert!(missing.is_current(&db).unwrap());
    db.meetings()
        .set_title_if_setting(&id, "Generated title", &missing)
        .unwrap();
    db.meetings()
        .insert_summary_if_setting(&id, summary("Generated summary"), &missing)
        .unwrap();
    db.save_sharing_document_if_setting(&id, 0, "generated document", &missing)
        .unwrap();
    db.put_setting("summarize", "off").unwrap();
    assert!(!missing.is_current(&db).unwrap());
}
