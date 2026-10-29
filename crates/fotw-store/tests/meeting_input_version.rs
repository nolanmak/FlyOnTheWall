//! Actual SQLite mutations must fence generated publication (#235).
use fotw_store::{Db, DbKey, NewSummary};

fn db() -> Db {
    Db::open_in_memory(&DbKey::from_bytes([17; 32])).unwrap()
}
fn seed(db: &Db, id: &str) {
    db.conn().execute(
        "INSERT INTO meetings(id,title,started_at_ms,tz,state,created_at,updated_at,origin_device_id)
         VALUES(?1,'Original title',0,'UTC','ready',0,0,'fixture')",[id]).unwrap();
    db.conn()
        .execute(
            "INSERT INTO notes(id,meeting_id,body_md,created_at,updated_at,origin_device_id)
         VALUES(?1,?2,'Original notes',0,0,'fixture')",
            [format!("notes-{id}"), id.into()],
        )
        .unwrap();
}
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
fn note_edits_refuse_all_generated_writes_without_demoting_previous_results() {
    let mut db = db();
    seed(&db, "meeting");
    let old = db
        .meetings()
        .insert_summary("meeting", summary("Original summary"))
        .unwrap();
    db.save_sharing_document("meeting", 0, "Original document")
        .unwrap();
    let version = db
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&db, "meeting")
        .unwrap();
    assert!(version.is_current(&db).unwrap());
    db.conn()
        .execute(
            "UPDATE notes SET body_md='New user notes' WHERE meeting_id='meeting'",
            [],
        )
        .unwrap();
    assert!(
        !version.is_current(&db).unwrap(),
        "notes changed while the model was working"
    );
    assert!(
        db.meetings()
            .set_title_if_setting("meeting", "Stale title", &version)
            .is_err()
    );
    assert!(
        db.meetings()
            .insert_summary_if_setting("meeting", summary("Stale summary"), &version)
            .is_err()
    );
    assert!(
        db.save_sharing_document_if_setting("meeting", 1, "Stale document", &version)
            .is_err()
    );
    assert_eq!(
        db.meetings().get("meeting").unwrap().title,
        "Original title"
    );
    assert_eq!(
        db.meetings()
            .current_summary("meeting")
            .unwrap()
            .unwrap()
            .id,
        old.id
    );
    assert_eq!(
        db.sharing_document("meeting").unwrap(),
        Some((1, "Original document".into()))
    );
    assert_eq!(
        db.note_document("meeting").unwrap().body_md,
        "New user notes"
    );
}
#[test]
fn changing_notes_away_and_back_cannot_revive_a_running_job() {
    let db = db();
    seed(&db, "meeting");
    let version = db
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&db, "meeting")
        .unwrap();
    db.conn()
        .execute(
            "UPDATE notes SET body_md='temporary' WHERE meeting_id='meeting'",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "UPDATE notes SET body_md='Original notes' WHERE meeting_id='meeting'",
            [],
        )
        .unwrap();
    assert!(!version.is_current(&db).unwrap());
}
#[test]
fn unrelated_edits_do_not_cancel_work_and_later_phases_bind_current_context() {
    let mut db = db();
    seed(&db, "one");
    seed(&db, "two");
    let version = db
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&db, "one")
        .unwrap();
    db.conn()
        .execute(
            "UPDATE notes SET body_md='Other notes' WHERE meeting_id='two'",
            [],
        )
        .unwrap();
    assert!(version.is_current(&db).unwrap());
    db.meetings()
        .set_title_if_setting("one", "Generated title", &version)
        .unwrap();
    assert!(
        !version.is_current(&db).unwrap(),
        "the generated title is context for later phases"
    );
    let version = db
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&db, "one")
        .unwrap();
    db.meetings()
        .insert_summary_if_setting("one", summary("Generated summary"), &version)
        .unwrap();
    db.save_sharing_document_if_setting("one", 0, "Generated document", &version)
        .unwrap();
    assert!(
        version.is_current(&db).unwrap(),
        "derived output is not a new transcript/notes input"
    );
}
#[test]
fn a_guard_cannot_publish_to_a_different_meeting() {
    let mut db = db();
    seed(&db, "one");
    seed(&db, "two");
    let version = db
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&db, "one")
        .unwrap();
    assert!(
        db.meetings()
            .set_title_if_setting("two", "Wrong meeting", &version)
            .is_err()
    );
    assert!(
        db.meetings()
            .insert_summary_if_setting("two", summary("Wrong meeting"), &version)
            .is_err()
    );
    assert!(
        db.save_sharing_document_if_setting("two", 0, "Wrong meeting", &version)
            .is_err()
    );
}
#[test]
fn delete_and_recreate_with_the_same_content_does_not_restore_an_old_guard() {
    let db = db();
    seed(&db, "meeting");
    let version = db
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&db, "meeting")
        .unwrap();
    db.conn()
        .execute("DELETE FROM meetings WHERE id='meeting'", [])
        .unwrap();
    assert!(!version.is_current(&db).unwrap());
    seed(&db, "meeting");
    assert!(!version.is_current(&db).unwrap());
}
#[test]
fn identical_content_in_another_library_does_not_share_a_guard() {
    let first = db();
    let second = db();
    seed(&first, "meeting");
    seed(&second, "meeting");
    let version = first
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&first, "meeting")
        .unwrap();
    assert!(!version.is_current(&second).unwrap());
}
#[test]
fn a_missing_meeting_cannot_be_bound_and_policy_changes_still_cancel_work() {
    let mut db = db();
    seed(&db, "meeting");
    assert!(
        db.setting_version("summarize")
            .unwrap()
            .with_meeting_inputs(&db, "missing")
            .is_err()
    );
    let version = db
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&db, "meeting")
        .unwrap();
    db.put_setting("summarize", r#"{"policy":"local_only"}"#)
        .unwrap();
    assert!(!version.is_current(&db).unwrap());
}

fn seed_transcript_context(db: &Db) {
    db.conn().execute_batch(r#"
        INSERT INTO transcripts(id,meeting_id,provider,model,created_at,updated_at,origin_device_id) VALUES('transcript','meeting','fixture','fixture',0,0,'fixture');
        INSERT INTO segments(id,transcript_id,meeting_id,idx,start_ms,end_ms,channel,text) VALUES('segment','transcript','meeting',0,0,1000,'system','Original words');
        INSERT INTO note_anchors(id,note_id,meeting_id,block_idx,block_text,typed_at_ms) VALUES('anchor','notes-meeting','meeting',0,'Original notes',0);
        INSERT INTO speech_epochs(id,meeting_id,transcript_id,session_id,source,epoch,from_sample,provider,model,capabilities_json) VALUES('epoch','meeting','transcript','session','system',0,0,'fixture','fixture','{}');
        INSERT INTO speech_receipts(id,meeting_id,transcript_id,epoch_id,from_sample,through_sample,payload_json) VALUES('receipt','meeting','transcript','epoch',0,16000,'{}');
        INSERT INTO session_note_links(session_id,meeting_id,started_at_ms,elapsed_ms,clock_is_final) VALUES('session','meeting',0,1000,1);
    "#).unwrap();
}
#[test]
fn every_transcript_note_and_recording_context_mutation_invalidates_observation() {
    for sql in [
        "UPDATE segments SET text='Changed words' WHERE id='segment'",
        "UPDATE transcripts SET language='en' WHERE id='transcript'",
        "UPDATE note_anchors SET typed_at_ms=42 WHERE id='anchor'",
        r#"UPDATE speech_epochs SET capabilities_json='{"changed":true}' WHERE id='epoch'"#,
        r#"UPDATE speech_receipts SET payload_json='{"changed":true}' WHERE id='receipt'"#,
        "UPDATE session_note_links SET clock_is_final=0 WHERE session_id='session'",
        "UPDATE meetings SET started_at_ms=1 WHERE id='meeting'",
        "UPDATE meetings SET ended_at_ms=2000 WHERE id='meeting'",
        "UPDATE meetings SET duration_ms=2000 WHERE id='meeting'",
        "UPDATE meetings SET tz='Europe/Berlin' WHERE id='meeting'",
        "UPDATE meetings SET title='Current title' WHERE id='meeting'",
        "DELETE FROM segments WHERE id='segment'",
        "DELETE FROM transcripts WHERE id='transcript'",
        "DELETE FROM notes WHERE id='notes-meeting'",
        "INSERT INTO segments(id,transcript_id,meeting_id,idx,start_ms,end_ms,channel,text) VALUES('later','transcript','meeting',1,1000,2000,'system','Later words')",
    ] {
        let db = db();
        seed(&db, "meeting");
        seed_transcript_context(&db);
        let v = db
            .setting_version("summarize")
            .unwrap()
            .with_meeting_inputs(&db, "meeting")
            .unwrap();
        db.conn().execute_batch(sql).unwrap();
        assert!(!v.is_current(&db).unwrap(), "{sql}");
    }
}
#[test]
fn rollback_preserves_observation_and_rebinding_cannot_refresh_a_stale_one() {
    let db = db();
    seed(&db, "meeting");
    let v = db
        .setting_version("summarize")
        .unwrap()
        .with_meeting_inputs(&db, "meeting")
        .unwrap();
    db.conn()
        .execute_batch("BEGIN; UPDATE notes SET body_md='Rolled back'; ROLLBACK;")
        .unwrap();
    assert!(v.is_current(&db).unwrap());
    db.conn()
        .execute("UPDATE notes SET body_md='Committed edit'", [])
        .unwrap();
    let still_old = v.with_meeting_inputs(&db, "meeting").unwrap();
    assert!(
        !still_old.is_current(&db).unwrap(),
        "a frozen job cannot refresh its own identity"
    );
    assert!(still_old.with_meeting_inputs(&db, "another").is_err());
}
#[test]
fn missing_or_damaged_generations_fail_closed_and_a_real_edit_rebuilds_them() {
    for damaged in [false, true] {
        let db = db();
        seed(&db, "meeting");
        let old = db
            .setting_version("summarize")
            .unwrap()
            .with_meeting_inputs(&db, "meeting")
            .unwrap();
        let sql = if damaged {
            "UPDATE app_meta SET value='damaged' WHERE key='summary_input_epoch:meeting'"
        } else {
            "DELETE FROM app_meta WHERE key='summary_input_epoch:meeting'"
        };
        db.conn().execute(sql, []).unwrap();
        assert!(!old.is_current(&db).unwrap_or(false));
        assert!(
            db.setting_version("summarize")
                .unwrap()
                .with_meeting_inputs(&db, "meeting")
                .is_err()
        );
        db.conn()
            .execute("UPDATE notes SET body_md='Current notes'", [])
            .unwrap();
        assert!(!old.is_current(&db).unwrap());
        let current = db
            .setting_version("summarize")
            .unwrap()
            .with_meeting_inputs(&db, "meeting")
            .unwrap();
        assert!(current.is_current(&db).unwrap());
    }
}
