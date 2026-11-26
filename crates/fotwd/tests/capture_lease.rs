use fotw_pipeline::{
    replay::{ReplaySource, ReplayWriter},
    wal::SessionWal,
};
use fotw_store::{Db, DbKey};
use fotw_stt::{Source, TranscriptSegment};
use fotwd::{
    failover::coverage::{CoverageJournal, EngineDescriptor},
    session::SessionOutcome,
    speech_recovery::SavedSpeechRecovery,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

struct Library {
    root: tempfile::TempDir,
    key: DbKey,
    database: PathBuf,
}

impl Library {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("db.sqlite3");
        let key = DbKey::from_bytes([0x4e; 32]);
        drop(Db::open(&database, &key).unwrap());
        Self {
            root,
            key,
            database,
        }
    }
    fn data_root(&self) -> &Path {
        self.root.path()
    }
    fn db(&self) -> Db {
        Db::open(&self.database, &self.key).unwrap()
    }
}

struct Capture {
    dir: PathBuf,
    session: String,
    started: u64,
    ended: u64,
}

/// A stopped, drained, finalized capture whose transcription journal covers
/// all of its input, the kind start-up recovery has always indexed.
fn capture(lib: &Library) -> Capture {
    let mut wal = SessionWal::create(lib.data_root().join("sessions"), 16000, 1).unwrap();
    let dir = wal.dir().to_owned();
    let session = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let started = wal.manifest().started_at_ms;
    wal.write_system(&vec![0.2; 24000]).unwrap();
    wal.checkpoint_capture(1500, true).unwrap();
    wal.finalize().unwrap();
    let mut audio = ReplayWriter::create(&dir, &session, ReplaySource::System, 0).unwrap();
    audio.append(&vec![900; 24000]).unwrap();
    audio.sync().unwrap();
    drop(audio);
    let mut journal = CoverageJournal::create(
        &dir,
        &session,
        Source::System,
        EngineDescriptor {
            provider: "fixture".into(),
            model: "synthetic".into(),
            revision: None,
            capabilities: fotw_stt::SttCapabilities {
                streaming: true,
                batch: false,
                word_timestamps: fotw_stt::FeatureAvailability::None,
                diarization: fotw_stt::FeatureAvailability::None,
                language_detection: false,
                custom_vocabulary: fotw_stt::CustomVocabulary::None,
                max_file_bytes: 0,
                max_file_seconds: 0,
                native_rates: vec![16000],
                retention_control: fotw_stt::RetentionControl::None,
                supports_replay_faster_than_realtime: false,
            },
        },
    )
    .unwrap();
    journal.commit(0, 0, 24000, vec![]).unwrap();
    drop(journal);
    Capture {
        dir,
        session,
        started,
        ended: started + 1500,
    }
}

#[test]
fn a_conflicting_live_save_keeps_the_log_edited_primary_and_user_title() {
    for journaled in [false, true] {
        let lib = Library::new();
        let c = capture(&lib);
        SavedSpeechRecovery::with_parts(
            lib.db(),
            lib.data_root().to_owned(),
            Arc::new(|_, _| Err("no fixture model".into())),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .index_captures()
        .unwrap();

        let mut db = lib.db();
        let id = db.session_note_meeting(&c.session).unwrap().unwrap();
        let primary = db
            .meetings()
            .create_transcript(&id, "fixture", "edited", true)
            .unwrap();
        db.meetings()
            .append_segments(
                &primary,
                &[fotw_store::NewSegment::new(0, 10, 50, "edited words")],
            )
            .unwrap();
        db.meetings().set_title(&id, "A prior title").unwrap();
        db.meetings().set_title(&id, "Recovered recording").unwrap();
        if !journaled {
            std::fs::remove_file(c.dir.join("system.stt-coverage.jsonl")).unwrap();
        }
        let mut segment = TranscriptSegment::new(&c.session, Source::System, "fixture", "live");
        segment.text = "line that only the live save has".into();
        segment.start_ms = 100;
        segment.end_ms = 200;
        segment.is_final = true;
        fotwd::session::append_segments(&c.dir, &[segment.clone()]).unwrap();
        let saved = fotwd::persist::persist_session_linked(
            &mut db,
            &SessionOutcome {
                dir: c.dir.clone(),
                started_at_ms: c.started,
                capture_ended_at_ms: Some(c.ended),
                segments: vec![segment],
                ..Default::default()
            },
            "Live title",
            None,
        );
        assert!(
            saved.is_err(),
            "existing alias was reported as a successful live save"
        );
        assert_eq!(db.meetings().get(&id).unwrap().title, "Recovered recording");
        let still_primary: bool = db
            .conn()
            .query_row(
                "SELECT is_primary FROM transcripts WHERE id=?1",
                [&primary],
                |row| row.get(0),
            )
            .unwrap();
        assert!(still_primary);
        assert_eq!(
            db.export_meeting(&id).unwrap().segments[0].text,
            "edited words"
        );
        assert!(
            std::fs::read_to_string(c.dir.join("stt.jsonl"))
                .unwrap()
                .contains("line that only the live save has")
        );
    }
}
