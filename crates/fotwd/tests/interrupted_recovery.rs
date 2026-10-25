//! Startup recovery uses only synthetic WALs, a temporary encrypted DB and fake inference.
use fotw_pipeline::{
    replay::{ReplayReader, ReplaySource, ReplayWriter},
    wal::SessionWal,
};
use fotw_store::{Db, DbKey, NoteDocument, SessionNoteDocument};
use fotw_stt::{Recognized, Recognizer, RecognizerFactory, Source, SttError};
use fotwd::{
    failover::coverage::{CoverageJournal, EngineDescriptor, LocalPlan},
    speech_recovery::SavedSpeechRecovery,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
struct Factory(Arc<AtomicUsize>);
struct Model;
impl RecognizerFactory for Factory {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Model))
    }
}
impl Recognizer for Model {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        Ok(vec![Recognized {
            text: "recovered after process exit".into(),
            start_ms: 0,
            end_ms: pcm.len() as u64 / 16,
        }])
    }
}
struct Fixture {
    root: tempfile::TempDir,
    key: DbKey,
    database: PathBuf,
    dir: PathBuf,
    session: String,
    start: u64,
    wal: Option<SessionWal>,
    calls: Arc<AtomicUsize>,
}
fn fixture(stopped: bool, canonical_samples: usize) -> Fixture {
    let raw = (0..48000)
        .map(|i| if i % 6400 < 3200 { 0.1 } else { 0.5 })
        .collect();
    fixture_audio(
        stopped,
        vec![4321; canonical_samples],
        raw,
        "fixture-local",
        "fixture-revision",
    )
}
fn fixture_audio(
    stopped: bool,
    canonical: Vec<i16>,
    raw: Vec<f32>,
    model: &str,
    revision: &str,
) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("db.sqlite3");
    let key = DbKey::from_bytes([0x6b; 32]);
    let mut wal = SessionWal::create(root.path().join("sessions"), 16000, 1).unwrap();
    let dir = wal.dir().to_owned();
    let session = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let start = wal.manifest().started_at_ms;
    wal.write_system(&raw).unwrap();
    let elapsed = raw.len() as u64 / 16 + 1000;
    wal.checkpoint_capture(elapsed, stopped).unwrap();
    let mut audio = ReplayWriter::create(&dir, &session, ReplaySource::System, 137 * 16).unwrap();
    audio.append(&canonical).unwrap();
    audio.sync().unwrap();
    drop(audio);
    let local = EngineDescriptor {
        provider: fotw_stt::local::PROVIDER.into(),
        model: model.into(),
        revision: Some(revision.into()),
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
            supports_replay_faster_than_realtime: true,
        },
    };
    let cloud = EngineDescriptor {
        provider: "deepgram".into(),
        model: "nova-3".into(),
        revision: None,
        capabilities: fotw_stt::SttCapabilities::deepgram(),
    };
    let mut journal = CoverageJournal::create_with_fallback(
        &dir,
        &session,
        Source::System,
        cloud,
        Some(LocalPlan {
            engine: local,
            language: Some("en".into()),
            endpoint: Default::default(),
        }),
    )
    .unwrap();
    let mut segment =
        fotw_stt::TranscriptSegment::new(&session, Source::System, "deepgram", "nova-3");
    segment.id = "committed-prefix".into();
    segment.text = "original committed words".into();
    segment.start_ms = 137;
    segment.end_ms = 637;
    segment.is_final = true;
    journal
        .commit(0, 137 * 16, 637 * 16, vec![segment])
        .unwrap();
    drop(journal);
    let mut db = Db::open(&database, &key).unwrap();
    db.save_session_note(
        &SessionNoteDocument {
            session_id: session.clone(),
            started_at_ms: start as i64,
            note: NoteDocument {
                revision: 0,
                body_md: "durable note before crash".into(),
                anchors: vec![],
            },
        },
        elapsed as i64,
    )
    .unwrap();
    Fixture {
        root,
        key,
        database,
        dir,
        session,
        start,
        wal: Some(wal),
        calls: Arc::new(AtomicUsize::new(0)),
    }
}
fn owner(f: &Fixture) -> SavedSpeechRecovery {
    let calls = f.calls.clone();
    SavedSpeechRecovery::with_parts(
        Db::open(&f.database, &f.key).unwrap(),
        f.root.path().to_owned(),
        Arc::new(move |plan, _| {
            assert_eq!(plan.engine.revision.as_deref(), Some("fixture-revision"));
            Ok(Arc::new(Factory(calls.clone())))
        }),
        Duration::from_secs(2),
        Duration::from_secs(5),
    )
}
fn startup(c: &SavedSpeechRecovery) {
    let pass = c.start_pending().unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while pass.is_running() {
        assert!(Instant::now() < deadline, "startup did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn startup_discovers_a_stopped_unfinalized_capture_and_recovers_pending_speech_once() {
    let mut f = fixture(true, 48000);
    drop(f.wal.take());
    let original = std::fs::read(f.dir.join("system.stt16")).unwrap();
    let raw = std::fs::read(f.dir.join("system.pcm")).unwrap();
    startup(&owner(&f));
    let mut db = Db::open(&f.database, &f.key).unwrap();
    let meeting = db
        .session_note_meeting(&f.session)
        .unwrap()
        .expect("startup must discover an unindexed stopped WAL");
    assert_eq!(
        db.session_note_clock(&f.session).unwrap(),
        Some((f.start as i64, 4000))
    );
    let saved = db.meetings().get(&meeting).unwrap();
    assert_eq!(saved.ended_at_ms, Some(f.start as i64 + 4000));
    assert_eq!(
        db.note_document(&meeting).unwrap().body_md,
        "durable note before crash"
    );
    assert!(f.calls.load(Ordering::SeqCst) > 0);
    assert_eq!(
        std::fs::read(f.dir.join("system.stt16")).unwrap(),
        original,
        "canonical PCM must never be rebuilt from quantized raw"
    );
    assert_eq!(std::fs::read(f.dir.join("system.pcm")).unwrap(), raw);
    assert!(
        !fotw_pipeline::wal::SessionState::read(&f.dir)
            .unwrap()
            .is_finalized(),
        "recovery must not manufacture a clean WAL Stop"
    );
    let mut reader = ReplayReader::open(&f.dir, &f.session, ReplaySource::System).unwrap();
    assert!(reader.is_complete().unwrap());
    assert_eq!(reader.read_from(137 * 16, 3).unwrap(), [4321; 3]);
    db.conn()
        .execute(
            "UPDATE segments SET text='user correction' WHERE id='committed-prefix'",
            [],
        )
        .unwrap();
    let count: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM segments", [], |r| r.get(0))
        .unwrap();
    let calls = f.calls.load(Ordering::SeqCst);
    drop(db);
    startup(&owner(&f));
    let db = Db::open(&f.database, &f.key).unwrap();
    assert_eq!(db.session_note_meeting(&f.session).unwrap(), Some(meeting));
    assert_eq!(
        db.conn()
            .query_row("SELECT count(*) FROM segments", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        count
    );
    assert_eq!(
        db.conn()
            .query_row(
                "SELECT text FROM segments WHERE id='committed-prefix'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "user correction"
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), calls);
}
#[test]
fn a_live_capture_lease_prevents_recovery_even_after_a_stop_request() {
    let f = fixture(true, 48000);
    let before = std::fs::read(f.dir.join("system.stt16")).unwrap();
    startup(&owner(&f));
    assert!(
        Db::open(&f.database, &f.key)
            .unwrap()
            .session_note_meeting(&f.session)
            .unwrap()
            .is_none()
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(f.dir.join("system.stt16")).unwrap(), before);
    assert!(!f.dir.join("system.stt-sealed.json").exists());
}
#[test]
fn an_unfrozen_clock_or_uncheckpointed_tail_never_manufactures_a_saved_end() {
    for stopped in [false, true] {
        let mut f = fixture(stopped, 48000);
        if stopped {
            wal_tail(&mut f);
        }
        drop(f.wal.take());
        startup(&owner(&f));
        assert!(
            Db::open(&f.database, &f.key)
                .unwrap()
                .session_note_meeting(&f.session)
                .unwrap()
                .is_none()
        );
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        assert!(
            !fotw_pipeline::wal::SessionState::read(&f.dir)
                .unwrap()
                .is_finalized()
        );
        assert!(!f.dir.join("system.stt-sealed.json").exists());
    }
}
fn wal_tail(f: &mut Fixture) {
    f.wal.as_mut().unwrap().write_system(&[0.2; 160]).unwrap();
    f.wal.as_mut().unwrap().flush().unwrap();
}
#[test]
fn a_missing_canonical_tail_is_retained_without_blind_raw_reconstruction() {
    let mut f = fixture(true, 16000);
    drop(f.wal.take());
    let before = std::fs::read(f.dir.join("system.stt16")).unwrap();
    startup(&owner(&f));
    assert!(
        Db::open(&f.database, &f.key)
            .unwrap()
            .session_note_meeting(&f.session)
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(f.dir.join("system.stt16")).unwrap(), before);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert!(!f.dir.join("system.stt-sealed.json").exists());
}

#[test]
fn another_source_writer_prevents_any_partial_freeze_or_projection() {
    let mut f = fixture(true, 48000);
    drop(f.wal.take());
    let held = CoverageJournal::open(&f.dir, &f.session, Source::System).unwrap();
    startup(&owner(&f));
    assert!(!f.dir.join("system.stt-sealed.json").exists());
    assert!(
        Db::open(&f.database, &f.key)
            .unwrap()
            .session_note_meeting(&f.session)
            .unwrap()
            .is_none()
    );
    drop(held);
    startup(&owner(&f));
    assert!(
        Db::open(&f.database, &f.key)
            .unwrap()
            .session_note_meeting(&f.session)
            .unwrap()
            .is_some()
    );
}
#[test]
fn corrupt_second_source_is_validated_before_the_first_source_is_frozen() {
    let mut f = fixture(true, 48000);
    drop(f.wal.take());
    drop(ReplayWriter::create(&f.dir, &f.session, ReplaySource::Mic, 0).unwrap());
    std::fs::write(
        f.dir.join("mic.stt-coverage.jsonl"),
        b"invalid complete record\n",
    )
    .unwrap();
    startup(&owner(&f));
    assert!(!f.dir.join("system.stt-sealed.json").exists());
    assert!(
        Db::open(&f.database, &f.key)
            .unwrap()
            .session_note_meeting(&f.session)
            .unwrap()
            .is_none()
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn cancelling_a_blocked_local_prepare_retains_the_interrupted_capture_lease_until_it_returns() {
    use fotw_web::speech_recovery::SpeechRecoveryControl;
    use std::sync::{Mutex, mpsc};
    let mut f = fixture(true, 48000);
    drop(f.wal.take());
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release = Mutex::new(release_rx);
    let calls = f.calls.clone();
    let c = SavedSpeechRecovery::with_parts(
        Db::open(&f.database, &f.key).unwrap(),
        f.root.path().to_owned(),
        Arc::new(move |_, _| {
            entered_tx.send(()).unwrap();
            release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(4))
                .map_err(|_| "fixture release timed out".to_owned())?;
            Ok(Arc::new(Factory(calls.clone())))
        }),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let pass = c.start_pending().unwrap().unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(fotw_pipeline::capture_clock::CaptureOwner::recover(&f.dir).is_err());
    drop(pass);
    assert!(
        fotw_pipeline::capture_clock::CaptureOwner::recover(&f.dir).is_err(),
        "cancellation must not revoke a retained worker's lease"
    );
    release_tx.send(()).unwrap();
    let db = Db::open(&f.database, &f.key).unwrap();
    let meeting = db.session_note_meeting(&f.session).unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while c.read(&meeting).unwrap().worker_running {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fotw_pipeline::capture_clock::CaptureOwner::recover(&f.dir).is_ok());
    assert_eq!(
        f.calls.load(Ordering::SeqCst),
        0,
        "cancelled preparation must never construct inference"
    );
}

/// Only the already approved, checksum-pinned public fixture and model may run.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires cached public small.en weights and checksum-verified JFK WAV"]
fn installed_model_recovers_an_unindexed_stopped_wal_without_changing_settings_or_audio() {
    native_capture_recovery(false);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires cached public small.en weights and checksum-verified JFK WAV"]
fn installed_model_recovers_a_finalized_archive_after_pcm_retirement() {
    native_capture_recovery(true);
}

#[cfg(target_os = "macos")]
fn native_capture_recovery(archived: bool) {
    use fotw_web::speech_recovery::{RecoveryPhase, SpeechRecoveryControl};
    use sha2::{Digest, Sha256};
    let model = PathBuf::from(
        std::env::var_os("FOTW_NATIVE_HANDOFF_MODEL").expect("public cached model path"),
    );
    let wav =
        PathBuf::from(std::env::var_os("FOTW_NATIVE_HANDOFF_WAV").expect("public cached WAV path"));
    let known = fotwd::stt::verify_model(&model).unwrap().unwrap();
    assert_eq!(known.name, "small.en");
    assert_eq!(
        known.sha256,
        "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d"
    );
    let bytes = std::fs::read(wav).unwrap();
    let digest = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(
        digest,
        "59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e"
    );
    let decoded = fotw_audio::wav::parse(&bytes).unwrap();
    assert_eq!(decoded.format.sample_rate_hz, 16000);
    assert_eq!(decoded.format.channels, 1);
    let canonical = decoded
        .samples
        .iter()
        .map(|s| (s * 32768.0).round() as i16)
        .collect();
    let mut f = fixture_audio(
        true,
        canonical,
        decoded.samples,
        "small.en",
        &format!("sha256:{}", known.sha256),
    );
    if archived {
        f.wal.take().unwrap().finalize().unwrap();
        fotw_pipeline::wal::encode_session(&f.dir).unwrap();
        fotw_pipeline::wal::discard_pcm(&f.dir).unwrap();
    } else {
        drop(f.wal.take());
    }
    let audio_before = std::fs::read(f.dir.join("system.stt16")).unwrap();
    let retained_path = f.dir.join(if archived {
        "system.opus"
    } else {
        "system.pcm"
    });
    let retained_before = std::fs::read(&retained_path).unwrap();
    let manifest_before = std::fs::read(f.dir.join("manifest.json")).unwrap();
    let mut db = Db::open(&f.database, &f.key).unwrap();
    let settings = fotwd::stt::SttSettings {
        engine: fotwd::stt::SttEngine::Deepgram,
        model: model.to_string_lossy().into_owned(),
        language: "fr".into(),
    };
    settings.write(&mut db).unwrap();
    let c = SavedSpeechRecovery::new(db, f.root.path().to_owned());
    let began = Instant::now();
    let pass = c.start_pending().unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while pass.is_running() {
        assert!(
            Instant::now() < deadline,
            "owned native recovery did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let db = Db::open(&f.database, &f.key).unwrap();
    let meeting = db
        .session_note_meeting(&f.session)
        .unwrap()
        .expect("interrupted WAL must become discoverable");
    assert_eq!(c.read(&meeting).unwrap().phase, RecoveryPhase::Complete);
    let snapshot = CoverageJournal::snapshot(&f.dir, &f.session, Source::System).unwrap();
    let recovered_capabilities = &snapshot.epochs.last().unwrap().engine.capabilities;
    assert_eq!(
        recovered_capabilities.word_timestamps,
        fotw_stt::FeatureAvailability::None
    );
    assert_eq!(
        recovered_capabilities.diarization,
        fotw_stt::FeatureAvailability::None
    );
    let text = snapshot
        .commits
        .iter()
        .flat_map(|c| &c.segments)
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    assert!(
        text.contains("country"),
        "public speech must actually be transcribed"
    );
    assert_eq!(fotwd::stt::SttSettings::read(&db).unwrap(), settings);
    assert_eq!(
        std::fs::read(f.dir.join("system.stt16")).unwrap(),
        audio_before
    );
    assert_eq!(std::fs::read(retained_path).unwrap(), retained_before);
    assert_eq!(
        std::fs::read(f.dir.join("manifest.json")).unwrap(),
        manifest_before
    );
    assert_eq!(
        fotw_pipeline::wal::SessionState::read(&f.dir)
            .unwrap()
            .is_finalized(),
        archived
    );
    if archived {
        assert!(!f.dir.join("system.pcm").exists());
    }
    println!(
        "{}",
        serde_json::json!({"fixture":"public-jfk","unindexed_stopped_wal":true,"raw_retired":archived,"phase":"complete","canonical_unchanged":true,"settings_unchanged":true,"elapsed_ms":began.elapsed().as_millis(),"model":"small.en"})
    );
}

#[cfg(unix)]
#[test]
fn a_dangling_source_journal_is_corruption_not_an_unconfigured_leg() {
    let mut f = fixture(true, 48000);
    drop(f.wal.take());
    std::os::unix::fs::symlink(
        f.root.path().join("absent-journal"),
        f.dir.join("mic.stt-coverage.jsonl"),
    )
    .unwrap();
    startup(&owner(&f));
    assert!(
        Db::open(&f.database, &f.key)
            .unwrap()
            .session_note_meeting(&f.session)
            .unwrap()
            .is_none()
    );
    assert!(!f.dir.join("system.stt-sealed.json").exists());
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn retained_interrupted_files_cannot_resurrect_a_deleted_saved_capture() {
    let mut f = fixture(true, 48000);
    drop(f.wal.take());
    startup(&owner(&f));
    let mut db = Db::open(&f.database, &f.key).unwrap();
    let meeting = db.session_note_meeting(&f.session).unwrap().unwrap();
    let held = f.root.path().join("held-capture");
    std::fs::rename(&f.dir, &held).unwrap();
    db.delete_meeting(&meeting).unwrap();
    std::fs::rename(held, &f.dir).unwrap();
    let calls = f.calls.load(Ordering::SeqCst);
    drop(db);
    startup(&owner(&f));
    let db = Db::open(&f.database, &f.key).unwrap();
    assert!(db.session_note_meeting(&f.session).unwrap().is_none());
    assert_eq!(
        db.conn()
            .query_row("SELECT count(*) FROM meetings", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), calls);
    assert!(f.dir.join("system.pcm").exists());
}

#[tokio::test]
async fn newly_imported_capture_announces_a_queryable_library_row_without_rebinding_live_history() {
    let mut f = fixture(true, 48000);
    drop(f.wal.take());
    let server = fotw_web::WebServer::bind(0, Arc::new(fotw_web::MemorySource::new()))
        .await
        .unwrap();
    let mut frames = server.state().hub().subscribe();
    let pass = owner(&f).install(server.state()).unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while pass.is_running() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let frame = frames
        .try_recv()
        .expect("an already-open dashboard must discover the imported row");
    let event: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(event["kind"], "meeting_ready");
    assert_eq!(event["reason"], "persisted");
    assert!(
        event.get("session_id").is_none(),
        "startup import must not evict or rebind a live capture's replay history"
    );
    let db = Db::open(&f.database, &f.key).unwrap();
    let meeting = db.session_note_meeting(&f.session).unwrap().unwrap();
    assert_eq!(event["meeting_id"], meeting);
    assert_eq!(
        db.conn()
            .query_row(
                "SELECT count(*) FROM meetings WHERE id=?1",
                [meeting],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn failed_prepare_keeps_capture_ownership_through_runtime_blocking_cleanup() {
    use std::sync::{Mutex, mpsc};
    for panics in [false, true] {
        let mut f = fixture(true, 48000);
        drop(f.wal.take());
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let release = Arc::new(Mutex::new(release_rx));
        let c = SavedSpeechRecovery::with_parts(
            Db::open(&f.database, &f.key).unwrap(),
            f.root.path().to_owned(),
            Arc::new(move |_, _| {
                let gate = release.clone();
                let entered = entered_tx.clone();
                tokio::task::spawn_blocking(move || {
                    entered.send(()).unwrap();
                    let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(4));
                });
                assert!(!panics, "fixture prepare panic with owned blocking cleanup");
                Err("fixture preparation failed with owned blocking cleanup".into())
            }),
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
        let pass = c.start_pending().unwrap().unwrap();
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let until = Instant::now() + Duration::from_millis(150);
        let mut escaped = false;
        while Instant::now() < until {
            if fotw_pipeline::capture_clock::CaptureOwner::recover(&f.dir).is_ok() {
                escaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while pass.is_running() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !escaped,
            "the async result returned but Runtime::drop still owned blocking cleanup"
        );
        assert!(fotw_pipeline::capture_clock::CaptureOwner::recover(&f.dir).is_ok());
    }
}

#[test]
fn startup_imports_a_finalized_unindexed_capture_before_or_after_pcm_retirement() {
    for (finalized, archived) in [(true, false), (true, true), (false, true)] {
        let mut f = fixture(true, 48000);
        let wal = f.wal.take().unwrap();
        let dir = if finalized {
            wal.finalize().unwrap()
        } else {
            let dir = wal.dir().to_owned();
            drop(wal);
            dir
        };
        if archived {
            fotw_pipeline::wal::encode_session(&dir).unwrap();
            fotw_pipeline::wal::discard_pcm(&dir).unwrap();
        }
        let canonical = std::fs::read(dir.join("system.stt16")).unwrap();
        let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
        let encoded = archived.then(|| std::fs::read(dir.join("system.opus")).unwrap());
        startup(&owner(&f));
        let mut db = Db::open(&f.database, &f.key).unwrap();
        let meeting = db
            .session_note_meeting(&f.session)
            .unwrap()
            .expect("a finalized WAL outside SQL must be discovered too");
        assert_eq!(
            db.session_note_clock(&f.session).unwrap(),
            Some((f.start as i64, 4000))
        );
        assert_eq!(
            db.note_document(&meeting).unwrap().body_md,
            "durable note before crash"
        );
        assert!(f.calls.load(Ordering::SeqCst) > 0);
        assert_eq!(std::fs::read(dir.join("system.stt16")).unwrap(), canonical);
        assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
        if let Some(encoded) = encoded {
            assert!(!dir.join("system.pcm").exists());
            assert_eq!(std::fs::read(dir.join("system.opus")).unwrap(), encoded);
        }
    }
}

#[test]
fn invalid_retired_archives_do_not_seal_input_project_notes_or_start_a_model() {
    for corrupt in [false, true] {
        let mut f = fixture(true, 48000);
        let dir = f.wal.take().unwrap().finalize().unwrap();
        fotw_pipeline::wal::encode_session(&dir).unwrap();
        fotw_pipeline::wal::discard_pcm(&dir).unwrap();
        let path = dir.join("system.opus");
        if corrupt {
            let mut bytes = std::fs::read(&path).unwrap();
            let n = bytes.len();
            bytes[n - 1] ^= 1;
            std::fs::write(&path, bytes).unwrap();
        } else {
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            let n = file.metadata().unwrap().len();
            file.set_len(n - 1).unwrap();
        }
        let canonical = std::fs::read(dir.join("system.stt16")).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        startup(&owner(&f));
        let mut db = Db::open(&f.database, &f.key).unwrap();
        assert!(db.session_note_meeting(&f.session).unwrap().is_none());
        assert!(db.session_note(&f.session).unwrap().is_some());
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        assert!(!dir.join("system.stt-sealed.json").exists());
        assert_eq!(std::fs::read(dir.join("system.stt16")).unwrap(), canonical);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
#[test]
fn finalized_capture_ownership_survives_cancelled_blocked_preparation() {
    use fotw_web::speech_recovery::SpeechRecoveryControl;
    use std::sync::{Mutex, mpsc};
    let mut f = fixture(true, 48000);
    let dir = f.wal.take().unwrap().finalize().unwrap();
    fotw_pipeline::wal::encode_session(&dir).unwrap();
    fotw_pipeline::wal::discard_pcm(&dir).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let gate = Mutex::new(release_rx);
    let calls = f.calls.clone();
    let c = SavedSpeechRecovery::with_parts(
        Db::open(&f.database, &f.key).unwrap(),
        f.root.path().to_owned(),
        Arc::new(move |_, _| {
            entered_tx.send(()).unwrap();
            gate.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(4))
                .unwrap();
            Ok(Arc::new(Factory(calls.clone())))
        }),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let pass = c.start_pending().unwrap().unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(fotw_pipeline::capture_clock::CaptureOwner::recover(&dir).is_err());
    drop(pass);
    assert!(fotw_pipeline::capture_clock::CaptureOwner::recover(&dir).is_err());
    release_tx.send(()).unwrap();
    let meeting = Db::open(&f.database, &f.key)
        .unwrap()
        .session_note_meeting(&f.session)
        .unwrap()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while c.read(&meeting).unwrap().worker_running {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fotw_pipeline::capture_clock::CaptureOwner::recover(&dir).is_ok());
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}
