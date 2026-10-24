//! Durable commit boundaries for the production handoff controller.
use fotw_pipeline::replay::{ReplaySource, ReplayWriter};
use fotw_stt::{Source, SttCapabilities, TranscriptSegment, Word};
use fotwd::failover::coverage::{CommitOutcome, CoverageJournal, EngineDescriptor, SwitchReason};

const LOCAL: &str = fotw_stt::local::PROVIDER;

fn engine(provider: &str, model: &str) -> EngineDescriptor {
    let mut capabilities = SttCapabilities::deepgram();
    if provider == LOCAL {
        capabilities.diarization = fotw_stt::FeatureAvailability::None;
        capabilities.word_timestamps = fotw_stt::FeatureAvailability::None;
        capabilities.custom_vocabulary = fotw_stt::CustomVocabulary::None;
        capabilities.retention_control = fotw_stt::RetentionControl::None;
        capabilities.native_rates = vec![16_000];
    }
    EngineDescriptor {
        provider: provider.into(),
        model: model.into(),
        revision: Some("fixture-v1".into()),
        capabilities,
    }
}
fn final_segment(id: &str, provider: &str, model: &str, start: u64, end: u64) -> TranscriptSegment {
    let mut segment = TranscriptSegment::new("s", Source::System, provider, model);
    segment.id = id.into();
    segment.is_final = true;
    segment.text = "yes yes please".into();
    segment.start_ms = start;
    segment.end_ms = end;
    segment
}
fn audio(dir: &std::path::Path, source: ReplaySource, anchor: u64) -> ReplayWriter {
    let mut writer = ReplayWriter::create(dir, "s", source, anchor).unwrap();
    writer.append(&vec![0; 75 * 16_000]).unwrap();
    writer
}

#[test]
fn handoff_keeps_committed_text_and_replays_uncommitted_audio_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let _audio = audio(dir.path(), ReplaySource::System, 137 * 16);
    let cloud = engine("deepgram", "nova-3");
    let mut journal = CoverageJournal::create(dir.path(), "s", Source::System, cloud).unwrap();
    let first = final_segment("stable-first", "deepgram", "nova-3", 237, 1_137);
    assert_eq!(
        journal
            .commit(0, 137 * 16, 1_137 * 16, vec![first.clone()])
            .unwrap(),
        CommitOutcome::New
    );
    let switched = journal
        .switch(engine(LOCAL, "small.en"), SwitchReason::Auth)
        .unwrap();
    assert_eq!(switched.number, 1);
    assert_eq!(switched.from_sample, 1_137 * 16);
    drop(journal);
    let mut recovered = CoverageJournal::open(dir.path(), "s", Source::System).unwrap();
    assert_eq!(recovered.committed_through(), 1_137 * 16);
    assert_eq!(recovered.current_epoch(), &switched);
    assert_eq!(recovered.commits()[0].segments, [first]);
    // The cloud's late result must not replace already committed local work,
    // or advance the pending interval before local inference has completed.
    assert!(
        recovered
            .commit(
                0,
                1_137 * 16,
                2_137 * 16,
                vec![final_segment("late", "deepgram", "nova-3", 1_137, 2_137)]
            )
            .is_err()
    );
    let local = final_segment("stable-local", LOCAL, "small.en", 1_137, 71_137);
    assert_eq!(
        recovered
            .commit(1, 1_137 * 16, 71_137 * 16, vec![local.clone()])
            .unwrap(),
        CommitOutcome::New
    );
    assert_eq!(
        recovered
            .commit(1, 1_137 * 16, 71_137 * 16, vec![local])
            .unwrap(),
        CommitOutcome::AlreadyCommitted
    );
    assert_eq!(recovered.commits().len(), 2);
    drop(recovered);
    let recovered = CoverageJournal::open(dir.path(), "s", Source::System).unwrap();
    assert_eq!(recovered.committed_through(), 71_137 * 16);
    assert_eq!(recovered.commits().len(), 2);
}

#[test]
fn rejects_holes_replacements_unsaved_audio_and_wrong_provider_identity() {
    let dir = tempfile::tempdir().unwrap();
    let _audio = audio(dir.path(), ReplaySource::System, 0);
    let mut journal = CoverageJournal::create(
        dir.path(),
        "s",
        Source::System,
        engine("deepgram", "nova-3"),
    )
    .unwrap();
    let first = final_segment("stable", "deepgram", "nova-3", 0, 1_000);
    assert!(journal.commit(0, 16, 16_000, vec![first.clone()]).is_err());
    assert!(
        journal
            .commit(0, 0, 76 * 16_000, vec![first.clone()])
            .is_err()
    );
    let mut wrong_source = first.clone();
    wrong_source.source = Source::Mic;
    assert!(journal.commit(0, 0, 16_000, vec![wrong_source]).is_err());
    let mut wrong_provider = first.clone();
    wrong_provider.provider = LOCAL.into();
    assert!(journal.commit(0, 0, 16_000, vec![wrong_provider]).is_err());
    let mut partial = first.clone();
    partial.is_final = false;
    assert!(journal.commit(0, 0, 16_000, vec![partial]).is_err());
    journal.commit(0, 0, 16_000, vec![first.clone()]).unwrap();
    let mut replaced = first;
    replaced.text = "changed".into();
    assert!(journal.commit(0, 0, 16_000, vec![replaced]).is_err());
    assert_eq!(journal.committed_through(), 16_000);
    assert_eq!(journal.commits().len(), 1);
}

#[test]
fn each_source_has_its_own_clock_epoch_and_pending_interval() {
    let dir = tempfile::tempdir().unwrap();
    let _system = audio(dir.path(), ReplaySource::System, 0);
    let _mic = audio(dir.path(), ReplaySource::Mic, 771 * 16);
    let mut system = CoverageJournal::create(
        dir.path(),
        "s",
        Source::System,
        engine("deepgram", "nova-3"),
    )
    .unwrap();
    let mic = CoverageJournal::create(dir.path(), "s", Source::Mic, engine("deepgram", "nova-3"))
        .unwrap();
    system.commit(0, 0, 1_000 * 16, vec![]).unwrap(); // acknowledged silence.
    system
        .switch(engine(LOCAL, "small.en"), SwitchReason::TransportBudget)
        .unwrap();
    assert_eq!(system.current_epoch().number, 1);
    assert_eq!(mic.current_epoch().number, 0);
    assert_eq!(mic.committed_through(), 771 * 16);
    assert!(
        CoverageJournal::open(dir.path(), "s", Source::Mic).is_err(),
        "only one journal owner"
    );
}

#[test]
fn recovery_repairs_only_an_incomplete_last_record_and_refuses_complete_corruption() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let _audio = audio(dir.path(), ReplaySource::System, 0);
    let mut journal = CoverageJournal::create(
        dir.path(),
        "s",
        Source::System,
        engine("deepgram", "nova-3"),
    )
    .unwrap();
    journal.commit(0, 0, 16_000, vec![]).unwrap();
    let path = journal.path().to_owned();
    drop(journal);
    let committed = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"kind\":\"commit\"")
        .unwrap();
    let recovered = CoverageJournal::open(dir.path(), "s", Source::System).unwrap();
    assert_eq!(recovered.committed_through(), 16_000);
    assert_eq!(std::fs::read(&path).unwrap(), committed);
    drop(recovered);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"kind\":\"commit\"}\n")
        .unwrap();
    let corrupted = std::fs::read(&path).unwrap();
    assert!(CoverageJournal::open(dir.path(), "s", Source::System).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), corrupted);
}

#[test]
fn invalid_word_offsets_and_nonfinite_confidence_cannot_enter_a_durable_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let _audio = audio(dir.path(), ReplaySource::System, 0);
    let mut journal = CoverageJournal::create(
        dir.path(),
        "s",
        Source::System,
        engine("deepgram", "nova-3"),
    )
    .unwrap();
    let mut segment = final_segment("bad-word", "deepgram", "nova-3", 0, 1_000);
    segment.words.push(Word {
        text: "outside".into(),
        start_ms: 0,
        end_ms: 2_000,
        confidence: None,
        speaker: None,
    });
    assert!(journal.commit(0, 0, 16_000, vec![segment]).is_err());
    let mut segment = final_segment("nonfinite", "deepgram", "nova-3", 0, 1_000);
    segment.confidence = Some(f64::NAN);
    assert!(journal.commit(0, 0, 16_000, vec![segment]).is_err());
    assert_eq!(journal.committed_through(), 0);
}

#[test]
fn oversized_receipts_do_not_change_the_log_and_a_bounded_retry_can_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let _audio = audio(dir.path(), ReplaySource::System, 0);
    let mut journal = CoverageJournal::create(
        dir.path(),
        "s",
        Source::System,
        engine("deepgram", "nova-3"),
    )
    .unwrap();
    let before = std::fs::read(journal.path()).unwrap();
    let mut huge = final_segment("huge", "deepgram", "nova-3", 0, 1_000);
    huge.text = "x".repeat(1_048_576);
    assert!(journal.commit(0, 0, 16_000, vec![huge]).is_err());
    assert_eq!(std::fs::read(journal.path()).unwrap(), before);
    assert_eq!(journal.committed_through(), 0);
    let bounded = final_segment("bounded", "deepgram", "nova-3", 0, 1_000);
    assert_eq!(
        journal.commit(0, 0, 16_000, vec![bounded]).unwrap(),
        CommitOutcome::New
    );
}
