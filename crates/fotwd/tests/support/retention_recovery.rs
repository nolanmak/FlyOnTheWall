pub fn archive_during_recovery(
    data_root: &std::path::Path,
    edit_policy: &str,
) -> (fotw_store::Db, Vec<std::path::PathBuf>) {
    use fotw_pipeline::{promote, wal::SessionWal};
    use fotw_store::{Db, DbKey, NewMeeting, NewSegment};

    let sessions = data_root.join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let mut db = Db::open_in_memory(&DbKey::from_bytes([7; 32])).unwrap();
    let now = fotw_store::now_ms();
    const DAY_MS: i64 = 86_400_000;
    let mut meeting = NewMeeting::new("fixture", "UTC");
    meeting.started_at_ms = now - 46 * DAY_MS;
    let id = db.meetings().create(meeting).unwrap();
    let transcript = db
        .meetings()
        .create_transcript(&id, "fixture", "fixture", true)
        .unwrap();
    db.meetings()
        .append_segments(
            &transcript,
            &[NewSegment {
                idx: 0,
                start_ms: 0,
                end_ms: 100,
                channel: "system".into(),
                speaker_label: None,
                person_id: None,
                text: "Check the agenda.".into(),
                confidence: None,
                is_final: true,
                words: None,
            }],
        )
        .unwrap();
    db.meetings().set_state(&id, "ready").unwrap();
    db.conn()
        .execute(
            "UPDATE transcripts SET created_at = ?2 WHERE id = ?1",
            (&transcript, now - 45 * DAY_MS),
        )
        .unwrap();
    let mut wal = SessionWal::create(&sessions, 16_000, 1).unwrap();
    wal.write_system(&[0.1; 1600]).unwrap();
    wal.write_mic(&[0.1; 1600]).unwrap();
    let session = wal.finalize().unwrap();
    promote::claim(&session, &id, (now - 46 * DAY_MS) as u64).unwrap();
    promote::encode(&session).unwrap();
    let archive = promote::publish(&session, data_root).unwrap();
    promote::stamp(&session, &archive).unwrap();
    db.conn().execute_batch(&format!(
        "CREATE TRIGGER edit_policy_during_recovery AFTER INSERT ON recordings BEGIN {edit_policy}; END;"
    )).unwrap();
    let tracks = archive
        .tracks
        .iter()
        .map(|track| data_root.join(&track.rel_path))
        .collect();
    (db, tracks)
}
