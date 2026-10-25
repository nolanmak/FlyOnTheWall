//! Saved-audio retries use an owned fake model and a temporary encrypted library.
use fotw_store::{Db, DbKey};
use fotw_stt::{Recognized, Recognizer, RecognizerFactory, Source, SttError};
use fotw_web::speech_recovery::{RecoveryPhase, SpeechRecoveryControl};
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
            text: "recovered pending speech".into(),
            start_ms: 0,
            end_ms: pcm.len() as u64 / 16,
        }])
    }
}
struct Fixture {
    root: tempfile::TempDir,
    dir: PathBuf,
    database: PathBuf,
    meeting: String,
    key: DbKey,
}
fn fixture() -> Fixture {
    let pcm = (0..48_000)
        .map(|n| if (n / 3200) % 2 == 0 { 1000 } else { 8000 })
        .collect();
    fixture_audio(pcm, "fixture-local", "fixture-revision")
}
fn fixture_audio(pcm: Vec<i16>, model: &str, revision: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let key = DbKey::from_bytes([0x6a; 32]);
    let database = root.path().join("owned.sqlite3");
    let (dir, meeting) = add_capture(
        root.path(),
        &database,
        &key,
        pcm,
        model,
        revision,
        "saved-prefix",
    );
    Fixture {
        root,
        dir,
        database,
        meeting,
        key,
    }
}
fn add_capture(
    root: &std::path::Path,
    database: &std::path::Path,
    key: &DbKey,
    pcm: Vec<i16>,
    model: &str,
    revision: &str,
    prefix_id: &str,
) -> (PathBuf, String) {
    use fotw_pipeline::{
        replay::{ReplaySource, ReplayWriter},
        wal::SessionWal,
    };
    let mut wal = SessionWal::create(root.join("sessions"), 16000, 1).unwrap();
    let dir = wal.dir().to_owned();
    let session = dir.file_name().unwrap().to_str().unwrap();
    wal.write_system(
        &pcm.iter()
            .map(|v| f32::from(*v) / 32768.0)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    wal.finalize().unwrap();
    // Synthetic but internally consistent original capture clock.
    let path = dir.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["started_at_ms"] = 1000.into();
    let ended = 1137 + pcm.len() as u64 / 16;
    manifest["ended_at_ms"] = ended.into();
    std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let mut audio = ReplayWriter::create(&dir, session, ReplaySource::System, 137 * 16).unwrap();
    audio.append(&pcm).unwrap();
    audio.seal().unwrap();
    let mut journal = CoverageJournal::create_with_fallback(
        &dir,
        session,
        Source::System,
        EngineDescriptor {
            provider: "deepgram".into(),
            model: "nova-3".into(),
            revision: None,
            capabilities: fotw_stt::SttCapabilities::deepgram(),
        },
        Some(LocalPlan {
            engine: EngineDescriptor {
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
            },
            language: Some("en".into()),
            endpoint: Default::default(),
        }),
    )
    .unwrap();
    let mut prefix =
        fotw_stt::TranscriptSegment::new(session, Source::System, "deepgram", "nova-3");
    prefix.id = prefix_id.into();
    prefix.text = "original prefix".into();
    prefix.is_final = true;
    prefix.start_ms = 137;
    prefix.end_ms = 637;
    journal.commit(0, 137 * 16, 637 * 16, vec![prefix]).unwrap();
    drop(journal);
    drop(audio);
    let mut db = Db::open(database, key).unwrap();
    let meeting = fotwd::persist::persist_session(
        &mut db,
        &fotwd::SessionOutcome {
            dir: dir.clone(),
            started_at_ms: 1000,
            capture_ended_at_ms: Some(ended),
            ..Default::default()
        },
        "Owned recovery",
    )
    .unwrap();
    db.conn()
        .execute(
            "UPDATE segments SET text='corrected prefix' WHERE id=?1",
            [prefix_id],
        )
        .unwrap();
    (dir, meeting)
}
fn controller(f: &Fixture, calls: Arc<AtomicUsize>) -> SavedSpeechRecovery {
    SavedSpeechRecovery::with_parts(
        Db::open(&f.database, &f.key).unwrap(),
        f.root.path().to_owned(),
        Arc::new(move |plan, _| {
            assert_eq!(plan.language.as_deref(), Some("en"));
            assert_eq!(plan.engine.revision.as_deref(), Some("fixture-revision"));
            Ok(Arc::new(Factory(calls.clone())))
        }),
        Duration::from_secs(2),
        Duration::from_secs(5),
    )
}
fn finished(c: &SavedSpeechRecovery, meeting: &str) -> fotw_web::speech_recovery::RecoveryStatus {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = c.read(meeting).unwrap();
        if !status.worker_running {
            return status;
        }
        assert!(Instant::now() < deadline, "{status:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn app_retry_recovers_pending_speech_once_and_preserves_edits_clock_and_original_audio() {
    let f = fixture();
    let pcm = std::fs::read(f.dir.join("system.pcm")).unwrap();
    let canonical = std::fs::read(f.dir.join("system.stt16")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = controller(&f, calls.clone());
    assert_eq!(c.read(&f.meeting).unwrap().phase, RecoveryPhase::Incomplete);
    c.retry(&f.meeting).unwrap();
    assert_eq!(finished(&c, &f.meeting).phase, RecoveryPhase::Complete);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let db = Db::open(&f.database, &f.key).unwrap();
    let clock = db
        .session_note_clock(f.dir.file_name().unwrap().to_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(clock, (1000, 3137));
    let prefix: String = db
        .conn()
        .query_row(
            "SELECT text FROM segments WHERE id='saved-prefix'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(prefix, "corrected prefix");
    let recovered: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM segments WHERE text='recovered pending speech'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(recovered > 0);
    c.retry(&f.meeting).unwrap();
    assert_eq!(finished(&c, &f.meeting).phase, RecoveryPhase::Complete);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "completed retry must not initialize another native model"
    );
    let count: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM segments WHERE text='recovered pending speech'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, recovered);
    assert_eq!(std::fs::read(f.dir.join("system.pcm")).unwrap(), pcm);
    assert_eq!(
        std::fs::read(f.dir.join("system.stt16")).unwrap(),
        canonical
    );
}

#[test]
fn invalid_saved_clock_and_deleted_meeting_never_initialize_a_model() {
    for deleted in [false, true] {
        let f = fixture();
        let mut db = Db::open(&f.database, &f.key).unwrap();
        if deleted {
            db.delete_meeting(&f.meeting).unwrap();
        } else {
            db.conn()
                .execute(
                    "UPDATE session_note_links SET started_at_ms=1001 WHERE meeting_id=?1",
                    [&f.meeting],
                )
                .unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let c = controller(&f, calls.clone());
        c.retry(&f.meeting).unwrap();
        assert_eq!(finished(&c, &f.meeting).phase, RecoveryPhase::Failed);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        if !deleted {
            assert!(f.dir.join("system.stt16").exists());
        }
    }
}

struct BlockingFactory {
    calls: Arc<AtomicUsize>,
    entered: std::sync::mpsc::Sender<()>,
    gate: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
    in_create: bool,
}
struct BlockingModel {
    entered: std::sync::mpsc::Sender<()>,
    gate: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
}
impl RecognizerFactory for BlockingFactory {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.in_create {
            self.entered.send(()).unwrap();
            self.gate.lock().unwrap().recv().unwrap();
            Ok(Box::new(Model))
        } else {
            Ok(Box::new(BlockingModel {
                entered: self.entered.clone(),
                gate: self.gate.clone(),
            }))
        }
    }
}
impl Recognizer for BlockingModel {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        self.entered.send(()).unwrap();
        self.gate.lock().unwrap().recv().unwrap();
        Model.transcribe(pcm)
    }
}
#[test]
fn timeout_or_cancel_retains_one_native_owner_and_fences_late_constructor_or_inference() {
    for (in_create, cancel) in [(true, false), (true, true), (false, true), (false, false)] {
        let f = fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, gate) = std::sync::mpsc::channel();
        let (entered, began) = std::sync::mpsc::channel();
        let factory = Arc::new(BlockingFactory {
            calls: calls.clone(),
            entered,
            gate: Arc::new(std::sync::Mutex::new(gate)),
            in_create,
        });
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed_clock = clock.clone();
        let c = SavedSpeechRecovery::with_parts(
            Db::open(&f.database, &f.key).unwrap(),
            f.root.path().to_owned(),
            Arc::new(move |_, _| Ok(factory.clone())),
            Duration::from_millis(30),
            Duration::from_secs(5),
        )
        .with_clock(Arc::new(move || {
            Duration::from_millis(observed_clock.load(Ordering::Acquire))
        }));
        c.retry(&f.meeting).unwrap();
        began.recv_timeout(Duration::from_secs(3)).unwrap();
        if cancel {
            c.cancel(&f.meeting).unwrap();
        } else {
            if !in_create {
                let deadline = Instant::now() + Duration::from_secs(3);
                while c.read(&f.meeting).unwrap().phase != RecoveryPhase::Transcribing {
                    assert!(
                        Instant::now() < deadline,
                        "inference phase was never admitted"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            clock.store(if in_create { 50 } else { 6000 }, Ordering::Release);
        }
        let pending = c.read(&f.meeting).unwrap();
        let duplicate = c.retry(&f.meeting);
        // Verify database responsiveness while a native constructor is blocked.
        let db = Db::open(&f.database, &f.key).unwrap();
        let prefix: String = db
            .conn()
            .query_row(
                "SELECT text FROM segments WHERE id='saved-prefix'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        release.send(()).unwrap();
        let ended = finished(&c, &f.meeting);
        assert_eq!(pending.phase, RecoveryPhase::Stopping);
        assert!(pending.worker_running && !pending.can_retry);
        assert_eq!(
            duplicate,
            Err(fotw_web::speech_recovery::RecoveryError::Busy)
        );
        assert_eq!(
            ended.phase,
            if cancel {
                RecoveryPhase::Cancelled
            } else {
                RecoveryPhase::Failed
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(prefix, "corrected prefix");
        let session = f.dir.file_name().unwrap().to_str().unwrap();
        let snapshot = CoverageJournal::snapshot(&f.dir, session, Source::System).unwrap();
        assert_eq!(
            snapshot.commits.len(),
            1,
            "late model output cannot commit after revocation"
        );
    }
}

/// Only the previously approved, checksum-pinned public assets are accepted.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires cached public small.en weights and checksum-verified JFK WAV"]
fn installed_model_recovers_saved_public_audio_through_the_app_controller() {
    use sha2::{Digest, Sha256};
    let model = PathBuf::from(
        std::env::var_os("FOTW_NATIVE_HANDOFF_MODEL").expect("set public model path"),
    );
    let wav =
        PathBuf::from(std::env::var_os("FOTW_NATIVE_HANDOFF_WAV").expect("set public WAV path"));
    let known = fotwd::stt::verify_model(&model).unwrap().unwrap();
    assert_eq!(known.name, "small.en");
    let bytes = std::fs::read(wav).unwrap();
    let digest = |bytes: &[u8]| {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert_eq!(
        digest(&bytes),
        "59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e"
    );
    let decoded = fotw_audio::wav::parse(&bytes).unwrap();
    assert_eq!(decoded.format.sample_rate_hz, 16000);
    assert_eq!(decoded.format.channels, 1);
    let pcm: Vec<i16> = decoded
        .samples
        .iter()
        .map(|s| (s * 32768.0).round() as i16)
        .collect();
    for valid_revision in [false, true] {
        let revision = if valid_revision {
            format!("sha256:{}", known.sha256)
        } else {
            format!("sha256:{}", "0".repeat(64))
        };
        let f = fixture_audio(pcm.clone(), "small.en", &revision);
        let mut db = Db::open(&f.database, &f.key).unwrap();
        let settings = fotwd::stt::SttSettings {
            engine: fotwd::stt::SttEngine::Deepgram,
            model: model.to_string_lossy().into_owned(),
            language: "fr".into(),
        };
        settings.write(&mut db).unwrap();
        let original = std::fs::read(f.dir.join("system.stt16")).unwrap();
        let c = SavedSpeechRecovery::new(db, f.root.path().to_owned());
        let began = Instant::now();
        let pass = c.start_pending().unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if !pass.is_running() {
                // Observe the final status after the completion barrier;
                // a status sampled before it can still be transitional.
                break c.read(&f.meeting).unwrap();
            }
            let status = c.read(&f.meeting).unwrap();
            assert!(Instant::now() < deadline, "{status:?}");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            status.phase,
            if valid_revision {
                RecoveryPhase::Complete
            } else {
                RecoveryPhase::Failed
            }
        );
        let db = Db::open(&f.database, &f.key).unwrap();
        assert_eq!(
            fotwd::stt::SttSettings::read(&db).unwrap(),
            settings,
            "saved recovery must not change today's cloud/language choice"
        );
        assert_eq!(std::fs::read(f.dir.join("system.stt16")).unwrap(), original);
        let snapshot = CoverageJournal::snapshot(
            &f.dir,
            f.dir.file_name().unwrap().to_str().unwrap(),
            Source::System,
        )
        .unwrap();
        if valid_revision {
            let text = snapshot
                .commits
                .iter()
                .flat_map(|c| c.segments.iter())
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(text.to_ascii_lowercase().contains("country"), "{text}");
            assert_eq!(status.sources[0].pending_audio_ms, 0);
        } else {
            assert_eq!(snapshot.commits.len(), 1);
        }
        eprintln!(
            "{}",
            serde_json::json!({"fixture":"public-jfk", "app_startup_recovery":true, "valid_revision":valid_revision, "phase":status.phase, "elapsed_ms":began.elapsed().as_millis(), "model":"small.en"})
        );
    }
}

struct PartialFactory {
    entered: std::sync::mpsc::Sender<()>,
    gate: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
}
struct PartialModel {
    owner: PartialFactory,
    chunks: usize,
}
impl RecognizerFactory for PartialFactory {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(PartialModel {
            owner: PartialFactory {
                entered: self.entered.clone(),
                gate: self.gate.clone(),
            },
            chunks: 0,
        }))
    }
}
impl Recognizer for PartialModel {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        self.chunks += 1;
        if self.chunks == 2 {
            self.owner.entered.send(()).unwrap();
            self.owner.gate.lock().unwrap().recv().unwrap();
        }
        Model.transcribe(pcm)
    }
}
#[test]
fn active_retry_reports_durable_catchup_before_the_whole_source_finishes() {
    let pcm = (0..73 * 16000)
        .map(|n| if (n / 3200) % 2 == 0 { 1000 } else { 8000 })
        .collect();
    let f = fixture_audio(pcm, "fixture-local", "fixture-revision");
    let (release, gate) = std::sync::mpsc::channel();
    let (entered, began) = std::sync::mpsc::channel();
    let model = Arc::new(PartialFactory {
        entered,
        gate: Arc::new(std::sync::Mutex::new(gate)),
    });
    let c = SavedSpeechRecovery::with_parts(
        Db::open(&f.database, &f.key).unwrap(),
        f.root.path().to_owned(),
        Arc::new(move |_, _| Ok(model.clone())),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .with_clock(Arc::new(|| Duration::ZERO));
    let before = c.read(&f.meeting).unwrap().sources[0].pending_audio_ms;
    c.retry(&f.meeting).unwrap();
    began.recv_timeout(Duration::from_secs(3)).unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    let pending = loop {
        let status = c.read(&f.meeting).unwrap();
        if status.sources[0].pending_audio_ms < before || Instant::now() > deadline {
            break status;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    release.send(()).unwrap();
    assert_eq!(finished(&c, &f.meeting).phase, RecoveryPhase::Complete);
    assert!(pending.worker_running);
    assert!(
        pending.sources[0].pending_audio_ms > 0 && pending.sources[0].pending_audio_ms < before,
        "status must report actual committed progress: {pending:?}"
    );
}

#[test]
fn confirmed_cloud_sources_need_no_local_plan_or_model_to_finalize_projection() {
    use fotw_pipeline::replay::{ReplayReader, ReplaySource};
    for complete_marker in [false, true] {
        let f = fixture();
        let session = f.dir.file_name().unwrap().to_str().unwrap();
        let mut reader = ReplayReader::open(&f.dir, session, ReplaySource::System).unwrap();
        let end = reader.saved_end().unwrap();
        let mut journal = CoverageJournal::open(&f.dir, session, Source::System).unwrap();
        journal
            .commit(0, journal.committed_through(), end, vec![])
            .unwrap();
        drop(journal);
        if complete_marker {
            reader.mark_complete(end).unwrap();
        }
        drop(reader);
        let path = f.dir.join("system.stt-coverage.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        let mut header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        header["header"].as_object_mut().unwrap().remove("fallback");
        let mut updated = serde_json::to_string(&header).unwrap() + "\n";
        for line in lines {
            updated.push_str(line);
            updated.push('\n');
        }
        std::fs::write(path, updated).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = controller(&f, calls.clone());
        assert_eq!(
            c.read(&f.meeting).unwrap().phase,
            RecoveryPhase::Incomplete,
            "confirmed inference still needs an acknowledged database projection"
        );
        c.retry(&f.meeting).unwrap();
        assert_eq!(finished(&c, &f.meeting).phase, RecoveryPhase::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            ReplayReader::open(&f.dir, session, ReplaySource::System)
                .unwrap()
                .is_projected()
                .unwrap()
        );
        assert_eq!(
            controller(&f, calls.clone())
                .read(&f.meeting)
                .unwrap()
                .phase,
            RecoveryPhase::Complete
        );
        assert!(
            ReplayReader::open(&f.dir, session, ReplaySource::System)
                .unwrap()
                .is_complete()
                .unwrap()
        );
        assert_eq!(
            CoverageJournal::snapshot(&f.dir, session, Source::System)
                .unwrap()
                .epochs
                .len(),
            1,
            "metadata-only completion cannot invent a provider switch"
        );
    }
}
struct SlowFactory;
struct SlowModel;
impl RecognizerFactory for SlowFactory {
    fn model(&self) -> &str {
        "fixture-local"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(SlowModel))
    }
}
impl Recognizer for SlowModel {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        std::thread::sleep(Duration::from_millis(100));
        Model.transcribe(pcm)
    }
}
#[test]
fn healthy_long_recovery_outlives_one_stall_budget_while_durable_progress_continues() {
    let pcm = (0..181 * 16000)
        .map(|n| if (n / 3200) % 2 == 0 { 1000 } else { 8000 })
        .collect();
    let f = fixture_audio(pcm, "fixture-local", "fixture-revision");
    let c = SavedSpeechRecovery::with_parts(
        Db::open(&f.database, &f.key).unwrap(),
        f.root.path().to_owned(),
        Arc::new(|_, _| Ok(Arc::new(SlowFactory))),
        Duration::from_secs(5),
        Duration::from_millis(500),
    );
    let began = Instant::now();
    c.retry(&f.meeting).unwrap();
    let status = finished(&c, &f.meeting);
    assert!(began.elapsed() > Duration::from_millis(500));
    assert_eq!(
        status.phase,
        RecoveryPhase::Complete,
        "healthy long recordings must not expire on a total-time cap: {status:?}"
    );
    assert_eq!(status.sources[0].pending_audio_ms, 0);
}

fn startup_finished(pass: &fotwd::speech_recovery::StartupRecovery) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while pass.is_running() {
        assert!(Instant::now() < deadline, "startup pass did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn startup_recovers_across_pages_once_and_reopening_skips_completed_models() {
    let f = fixture();
    let mut meetings = vec![f.meeting.clone()];
    for n in 0..33 {
        let (_, meeting) = add_capture(
            f.root.path(),
            &f.database,
            &f.key,
            vec![1000; 16000],
            "fixture-local",
            "fixture-revision",
            &format!("prefix-{n}"),
        );
        meetings.push(meeting);
    }
    let original = std::fs::read(f.dir.join("system.pcm")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = controller(&f, calls.clone());
    let pass = c.start_pending().unwrap().unwrap();
    startup_finished(&pass);
    assert_eq!(calls.load(Ordering::SeqCst), meetings.len());
    assert!(
        c.start_pending().unwrap().is_none(),
        "one pass per controller"
    );
    for meeting in &meetings {
        assert_eq!(c.read(meeting).unwrap().phase, RecoveryPhase::Complete);
    }
    let reopened = controller(&f, calls.clone());
    let next = reopened.start_pending().unwrap().unwrap();
    startup_finished(&next);
    assert_eq!(calls.load(Ordering::SeqCst), meetings.len());
    assert_eq!(std::fs::read(f.dir.join("system.pcm")).unwrap(), original);
    let db = Db::open(&f.database, &f.key).unwrap();
    let prefix: String = db
        .conn()
        .query_row(
            "SELECT text FROM segments WHERE id='saved-prefix'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(prefix, "corrected prefix");
    assert_eq!(
        db.session_note_clock(f.dir.file_name().unwrap().to_str().unwrap())
            .unwrap(),
        Some((1000, 3137))
    );
}

#[test]
fn startup_skips_invalid_capture_and_missing_models_without_starving_later_work() {
    let f = fixture();
    let (_, bad) = add_capture(
        f.root.path(),
        &f.database,
        &f.key,
        vec![1000; 16000],
        "fixture-local",
        "fixture-revision",
        "bad-clock",
    );
    let (_, missing) = add_capture(
        f.root.path(),
        &f.database,
        &f.key,
        vec![1000; 16000],
        "missing-model",
        "saved-missing-revision",
        "missing-model-prefix",
    );
    let (_, good) = add_capture(
        f.root.path(),
        &f.database,
        &f.key,
        vec![1000; 16000],
        "fixture-local",
        "fixture-revision",
        "later-good",
    );
    let db = Db::open(&f.database, &f.key).unwrap();
    db.conn()
        .execute(
            "UPDATE session_note_links SET started_at_ms=1001 WHERE meeting_id=?1",
            [&bad],
        )
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let models = calls.clone();
    let c = SavedSpeechRecovery::with_parts(
        db,
        f.root.path().to_owned(),
        Arc::new(move |plan, _| {
            assert_eq!(plan.language.as_deref(), Some("en"));
            if plan.engine.model == "missing-model" {
                return Err("fixture model not installed".into());
            }
            assert_eq!(plan.engine.revision.as_deref(), Some("fixture-revision"));
            Ok(Arc::new(Factory(models.clone())))
        }),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let pass = c.start_pending().unwrap().unwrap();
    startup_finished(&pass);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(c.read(&f.meeting).unwrap().phase, RecoveryPhase::Complete);
    assert_eq!(c.read(&good).unwrap().phase, RecoveryPhase::Complete);
    assert!(c.read(&missing).unwrap().can_retry);
    assert!(c.read(&bad).is_err());
}

#[test]
fn explicit_retry_before_startup_keeps_automatic_recovery_from_taking_control() {
    let f = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = controller(&f, calls.clone());
    c.retry(&f.meeting).unwrap();
    assert_eq!(finished(&c, &f.meeting).phase, RecoveryPhase::Complete);
    assert!(c.start_pending().unwrap().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn startup_cancel_or_shutdown_fences_output_and_retains_native_admission() {
    for cancel in [true, false] {
        let f = fixture();
        let (_, second) = add_capture(
            f.root.path(),
            &f.database,
            &f.key,
            vec![1000; 16000],
            "fixture-local",
            "fixture-revision",
            "queued-prefix",
        );
        let db = Db::open(&f.database, &f.key).unwrap();
        let first: String = db
            .conn()
            .query_row(
                "SELECT meeting_id FROM session_note_links ORDER BY session_id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let other = if first == f.meeting {
            second
        } else {
            f.meeting.clone()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, gate) = std::sync::mpsc::channel();
        let (entered, began) = std::sync::mpsc::channel();
        let factory = Arc::new(BlockingFactory {
            calls: calls.clone(),
            entered,
            gate: Arc::new(std::sync::Mutex::new(gate)),
            in_create: true,
        });
        let c = SavedSpeechRecovery::with_parts(
            db,
            f.root.path().to_owned(),
            Arc::new(move |_, _| Ok(factory.clone())),
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
        let mut pass = c.start_pending().unwrap();
        began
            .recv_timeout(Duration::from_secs(3))
            .expect("startup did not initialize the saved model");
        assert!(c.start_pending().unwrap().is_none());
        if cancel {
            c.cancel(&first).unwrap();
        } else {
            drop(pass.take());
        }
        assert_eq!(c.read(&first).unwrap().phase, RecoveryPhase::Stopping);
        assert_eq!(
            c.retry(&other),
            Err(fotw_web::speech_recovery::RecoveryError::Busy)
        );
        release.send(()).unwrap();
        assert_eq!(finished(&c, &first).phase, RecoveryPhase::Cancelled);
        if let Some(pass) = pass {
            startup_finished(&pass);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "queued work must not restart after cancellation"
        );
        let db = Db::open(&f.database, &f.key).unwrap();
        let late: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM segments WHERE text='recovered pending speech'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(late, 0);
    }
}

#[tokio::test]
async fn daemon_install_starts_saved_work_and_exposes_the_same_controller() {
    let f = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = controller(&f, calls.clone());
    let server = fotw_web::WebServer::bind(0, Arc::new(fotw_web::MemorySource::new()))
        .await
        .unwrap();
    let pass = c
        .install(server.state())
        .unwrap()
        .expect("daemon startup must own a recovery pass");
    startup_finished(&pass);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let control = server.state().speech_recovery().unwrap();
    assert_eq!(
        control.read(&f.meeting).unwrap().phase,
        RecoveryPhase::Complete
    );
}

#[test]
fn startup_horizon_does_not_chase_recordings_saved_after_the_scan_begins() {
    let f = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let (release, gate) = std::sync::mpsc::channel();
    let (entered, began) = std::sync::mpsc::channel();
    let factory = Arc::new(BlockingFactory {
        calls: calls.clone(),
        entered,
        gate: Arc::new(std::sync::Mutex::new(gate)),
        in_create: true,
    });
    let c = SavedSpeechRecovery::with_parts(
        Db::open(&f.database, &f.key).unwrap(),
        f.root.path().to_owned(),
        Arc::new(move |_, _| Ok(factory.clone())),
        Duration::from_secs(5),
        Duration::from_secs(5),
    );
    let pass = c.start_pending().unwrap().unwrap();
    began.recv_timeout(Duration::from_secs(3)).unwrap();
    let (late_dir, late) = add_capture(
        f.root.path(),
        &f.database,
        &f.key,
        vec![1000; 16000],
        "fixture-local",
        "fixture-revision",
        "post-startup",
    );
    assert!(
        late_dir.file_name() > f.dir.file_name(),
        "fixture must be beyond startup's saved horizon"
    );
    release.send(()).unwrap();
    startup_finished(&pass);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(c.read(&f.meeting).unwrap().phase, RecoveryPhase::Complete);
    assert_eq!(c.read(&late).unwrap().phase, RecoveryPhase::Incomplete);
}
