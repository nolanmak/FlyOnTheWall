//! Durable projection retries preserve edited text while appending recovered tails.
use fotw_store::{
    Db, NewMeeting, NewSegment, SpeechCommit, SpeechEpoch, SpeechSegment, SpeechSource,
};
mod common;

fn source(channel: &str) -> SpeechSource {
    SpeechSource {
        source: channel.into(),
        epochs: vec![SpeechEpoch {
            number: 0,
            from_sample: 0,
            provider: "fixture-cloud".into(),
            model: "fixture-nova".into(),
            revision: None,
            capabilities: serde_json::json!({"diarization":"provider"}),
            reason: None,
        }],
        commits: vec![SpeechCommit {
            epoch: 0,
            from_sample: 0,
            through_sample: 16_000,
            segments: vec![SpeechSegment {
                id: format!("original-{channel}"),
                timestamp_source: "provider".into(),
                words: vec![],
                row: NewSegment::new(0, 0, 1_000, "yes yes please").channel(channel),
            }],
        }],
    }
}
fn save(db: &mut Db, sources: &[SpeechSource]) -> fotw_store::Result<String> {
    db.finalize_speech_capture(
        "capture-fixture",
        NewMeeting::new("fixture-device", "UTC").started_at_ms(1_000),
        5_000,
        sources,
    )
}
#[test]
fn retry_after_restart_appends_tail_and_preserves_edits_and_both_sources() {
    let (_root, path) = common::tmp_db();
    let key = common::test_key();
    let mut db = Db::open(&path, &key).unwrap();
    let mut sources = vec![source("system"), source("mic")];
    let meeting = save(&mut db, &sources).unwrap();
    db.conn()
        .execute(
            "UPDATE segments SET text='user correction' WHERE id='original-system'",
            [],
        )
        .unwrap();
    db.meetings().set_title(&meeting, "Edited title").unwrap();
    let alternate = db
        .meetings()
        .create_transcript(&meeting, "fixture-alternate", "owned-model", true)
        .unwrap();
    let revision = db
        .save_note_document(&meeting, 0, "owned note", &[])
        .unwrap();
    assert_eq!(save(&mut db, &sources).unwrap(), meeting);
    drop(db);
    sources[0].epochs.push(SpeechEpoch {
        number: 1,
        from_sample: 16_000,
        provider: "fixture-local".into(),
        model: "small.en".into(),
        revision: Some("fixture-sha".into()),
        capabilities: serde_json::json!({"diarization":"none"}),
        reason: Some("transport_budget".into()),
    });
    sources[0].commits.push(SpeechCommit {
        epoch: 1,
        from_sample: 16_000,
        through_sample: 32_000,
        segments: vec![SpeechSegment {
            id: "recovered-system".into(),
            timestamp_source: "estimated".into(),
            words: vec![],
            row: NewSegment::new(0, 1_000, 2_000, "recovered tail").channel("system"),
        }],
    });
    let mut db = Db::open(&path, &key).unwrap();
    assert_eq!(save(&mut db, &sources).unwrap(), meeting);
    assert_eq!(save(&mut db, &sources).unwrap(), meeting);
    let doc = db.export_meeting(&meeting).unwrap();
    assert_eq!(doc.segments.len(), 3);
    assert_eq!(
        doc.segments
            .iter()
            .find(|s| s.id == "original-system")
            .unwrap()
            .text,
        "user correction"
    );
    assert!(
        doc.segments
            .iter()
            .any(|s| s.id == "original-mic" && s.text == "yes yes please")
    );
    assert_eq!(doc.speech_epochs.len(), 3);
    assert_eq!(
        db.meetings()
            .primary_transcript_id(&meeting)
            .unwrap()
            .as_deref(),
        Some(alternate.as_str())
    );
    assert_eq!(doc.speech_receipts.len(), 3);
    assert_eq!(db.meetings().get(&meeting).unwrap().title, "Edited title");
    assert_eq!(db.note_document(&meeting).unwrap().revision, revision);
    assert_eq!(db.note_document(&meeting).unwrap().body_md, "owned note");
}
#[test]
fn conflicting_receipt_or_identity_rolls_back_without_replacing_saved_text() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let original = source("system");
    let meeting = save(&mut db, std::slice::from_ref(&original)).unwrap();
    let before = db.export_meeting(&meeting).unwrap();
    let mut changed = original.clone();
    changed.commits[0].segments[0].row.text = "different inference".into();
    assert!(save(&mut db, &[changed]).is_err());
    assert_eq!(db.export_meeting(&meeting).unwrap(), before);
    let mut changed = original;
    changed.epochs[0].model = "another-model".into();
    assert!(save(&mut db, &[changed]).is_err());
    assert_eq!(db.export_meeting(&meeting).unwrap(), before);
}
#[test]
fn gaps_bad_source_and_stale_epoch_never_leave_partial_projection() {
    for failure in ["gap", "bad-source", "stale-epoch", "duplicate-id"] {
        let mut db = Db::open_in_memory(&common::test_key()).unwrap();
        let mut data = source("system");
        match failure {
            "gap" => data.commits[0].from_sample = 160,
            "bad-source" => data.source = "other".into(),
            "stale-epoch" => data.commits[0].epoch = 2,
            "duplicate-id" => {
                let duplicate = data.commits[0].segments[0].clone();
                data.commits[0].segments.push(duplicate);
            }
            _ => unreachable!(),
        }
        assert!(save(&mut db, &[data]).is_err(), "{failure}");
        assert!(db.meetings().list(10, 0).unwrap().is_empty(), "{failure}");
        assert!(
            db.session_note_meeting("capture-fixture")
                .unwrap()
                .is_none()
        );
    }
}
#[test]
fn deleting_capture_cascades_receipts_and_prevents_late_recovery() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let sources = vec![source("system")];
    let meeting = save(&mut db, &sources).unwrap();
    db.delete_meeting(&meeting).unwrap();
    assert!(save(&mut db, &sources).is_err());
    for table in ["speech_epochs", "speech_receipts", "segments"] {
        assert_eq!(
            db.conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn archive_import_retains_original_receipts_and_retries_do_not_erase_corrections() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let sources = vec![source("system")];
    let meeting = save(&mut db, &sources).unwrap();
    db.conn()
        .execute("UPDATE segments SET text='owned correction'", [])
        .unwrap();
    let archive_root = tempfile::tempdir().unwrap();
    let archive = archive_root.path().join("archive");
    db.export_library(
        &archive,
        &fotw_store::ArchiveOptions::default(),
        &mut |_| {},
    )
    .unwrap();
    let before = db.export_meeting(&meeting).unwrap();
    let mut restored = Db::open_in_memory(&common::test_key()).unwrap();
    restored.import_library(&archive).unwrap();
    assert_eq!(save(&mut restored, &sources).unwrap(), meeting);
    assert_eq!(restored.export_meeting(&meeting).unwrap(), before);
    let mut old = serde_json::to_value(&before).unwrap();
    old.as_object_mut().unwrap().remove("speech_epochs");
    old.as_object_mut().unwrap().remove("speech_receipts");
    let old: fotw_store::MeetingDoc = serde_json::from_value(old).unwrap();
    assert!(old.speech_epochs.is_empty() && old.speech_receipts.is_empty());
}

#[test]
fn failed_second_source_insertion_rolls_back_every_table() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    db.conn().execute_batch("CREATE TRIGGER reject_mic BEFORE INSERT ON segments WHEN new.channel='mic' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    assert!(save(&mut db, &[source("system"), source("mic")]).is_err());
    for table in [
        "meetings",
        "transcripts",
        "segments",
        "speech_epochs",
        "speech_receipts",
        "session_note_links",
    ] {
        assert_eq!(
            db.conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0,
            "{table}"
        );
    }
}

#[test]
fn receipt_identity_uses_original_words_instead_of_compression_bytes() {
    let mut db = Db::open_in_memory(&common::test_key()).unwrap();
    let mut data = source("system");
    data.commits[0].segments[0].row.words = Some(vec![1, 2, 3]);
    data.commits[0].segments[0].words =
        vec![serde_json::json!({"text":"yes","startMs":0,"endMs":1000})];
    let meeting = save(&mut db, std::slice::from_ref(&data)).unwrap();
    data.commits[0].segments[0].row.words = Some(vec![4, 5, 6]);
    assert_eq!(save(&mut db, std::slice::from_ref(&data)).unwrap(), meeting);
    assert_eq!(
        db.export_meeting(&meeting).unwrap().segments[0]
            .words
            .as_ref()
            .unwrap()
            .0,
        [1, 2, 3]
    );
    data.commits[0].segments[0].words[0]["text"] = "different word".into();
    assert!(save(&mut db, &[data]).is_err());
}
