//! Parakeet Unified on a recording's durable local source (#244), served by
//! the model-free `fotw-speechd-fake` in place of the Swift helper.
//!
//! What a meeting relies on: every segment's provenance names Parakeet and the
//! revision the helper loaded, and a helper that dies mid-meeting ends that
//! source's transcription with a clear error, promptly, with its audio kept.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use fotw_pipeline::replay::{ReplayReader, ReplaySource};
use fotw_stt::{LocalStreamConfig, Source};
use fotwd::failover::coverage::CoverageJournal;
use fotwd::failover::stream::{FailoverStream, LocalSourceConfig, Phase};
use fotwd::speechd::{ENGINE, Launch, REVISION};

/// The model-free helper, rebuilt if its source changed.
fn fake_helper() -> PathBuf {
    fotw_speechd_client::testing::fake_helper()
}

fn launch() -> Launch {
    Launch {
        program: fake_helper(),
        extra_args: vec![
            "--fake-engines".into(),
            "--pinned".into(),
            format!("{ENGINE}@{REVISION}").into(),
        ],
    }
}

/// Explicit setup blocks, so it runs on a worker, as the CLI and settings run it.
async fn install(data_root: &std::path::Path) {
    let data_root = data_root.to_owned();
    tokio::task::spawn_blocking(move || {
        fotwd::speechd::install(&launch(), &data_root, None, &|| false, &mut |_| {})
    })
    .await
    .unwrap()
    .unwrap();
}

/// Two seconds of syllable-like level swings, then silence to end it.
fn utterance() -> Vec<i16> {
    let mut pcm: Vec<i16> = (0..32_000)
        .map(|n| if (n / 480) % 2 == 0 { 1_000 } else { 8_000 })
        .collect();
    pcm.extend(std::iter::repeat_n(0, 16_000));
    pcm
}

fn config(
    engine: std::sync::Arc<dyn fotw_stt::RecognizerFactory>,
    revision: String,
) -> LocalSourceConfig {
    let mut config = LocalSourceConfig::new(
        LocalStreamConfig::new("fixture", Source::System).with_session_offset_ms(137),
        engine,
    );
    config.revision = Some(revision);
    config.stop_timeout = Duration::from_secs(5);
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_segment_records_parakeet_and_the_revision_the_helper_loaded() {
    let data_root = tempfile::tempdir().unwrap();
    install(data_root.path()).await;
    let (engine, revision) = fotwd::speechd::prepare(&launch(), data_root.path())
        .await
        .unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (stream, _) = FailoverStream::open_local(sessions.path(), config(engine, revision))
        .await
        .unwrap();
    stream.write(&utterance()).unwrap();
    assert!(stream.finish().await, "{:?}", stream.status());

    let saved = CoverageJournal::snapshot(sessions.path(), "fixture", Source::System).unwrap();
    let engine = &saved.epochs[0].engine;
    assert_eq!(engine.provider, fotw_stt::speechd::PROVIDER);
    assert_eq!(
        format!(
            "{}@{}",
            engine.model,
            engine.revision.as_deref().unwrap_or("?")
        ),
        "parakeet-unified@d32e972dd4315f1dc3f6be28fb2aab0ab3e80358"
    );
    let segments: Vec<_> = saved.commits.iter().flat_map(|c| &c.segments).collect();
    assert_eq!(segments.len(), 1, "{segments:?}");
    assert_eq!(segments[0].provider, fotw_stt::speechd::PROVIDER);
    assert_eq!(segments[0].model, ENGINE);
    assert!(segments[0].text.starts_with("samples="), "{segments:?}");
    // Speech from the first sample, on a clock that starts at 137 ms: the
    // segment starts there and ends inside the utterance's 3 s.
    assert_eq!(segments[0].start_ms, 137, "{segments:?}");
    assert!(
        (137 + 2_000..=137 + 3_000).contains(&segments[0].end_ms),
        "{segments:?}"
    );
}

/// A sample value the fake helper is told to die on (`--crash-on-sample`).
const POISON: i16 = 4_321;

/// An utterance that kills the helper whenever it is sent.
fn poisoned() -> Vec<i16> {
    let mut pcm = utterance();
    pcm[16_000] = POISON;
    pcm
}

fn crashing_launch(marker: Option<&std::path::Path>) -> Launch {
    let mut launch = launch();
    launch.extra_args.push("--crash-on-sample".into());
    launch.extra_args.push(POISON.to_string().into());
    if let Some(marker) = marker {
        launch.extra_args.push("--crash-once-marker".into());
        launch.extra_args.push(marker.as_os_str().to_owned());
    }
    launch
}

/// Records `parts` back to back through Parakeet in `launch`'s helper.
async fn record(
    launch: &Launch,
    parts: &[Vec<i16>],
) -> (FailoverStream, bool, tempfile::TempDir, Vec<i16>) {
    let data_root = tempfile::tempdir().unwrap();
    install(data_root.path()).await;
    let (engine, revision) = fotwd::speechd::prepare(launch, data_root.path())
        .await
        .unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (stream, _) = FailoverStream::open_local(sessions.path(), config(engine, revision))
        .await
        .unwrap();
    let audio: Vec<i16> = parts.concat();
    stream.write(&audio).unwrap();
    let finished = tokio::time::timeout(Duration::from_secs(30), stream.finish())
        .await
        .expect("a failing helper never holds up the end of a recording");
    (stream, finished, sessions, audio)
}

fn retained(sessions: &std::path::Path, len: usize) -> Vec<i16> {
    let mut reader = ReplayReader::open(sessions, "fixture", ReplaySource::System).unwrap();
    let mut retained = Vec::new();
    while retained.len() < len {
        let next = reader
            .read_from(137 * 16 + retained.len() as u64, 16_000)
            .unwrap();
        if next.is_empty() {
            break;
        }
        retained.extend(next);
    }
    retained
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_helper_that_crashes_once_mid_meeting_loses_no_utterance() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("crashed");
    let (stream, finished, sessions, audio) = record(
        &crashing_launch(Some(&marker)),
        &[utterance(), poisoned(), utterance()],
    )
    .await;
    assert!(
        marker.exists(),
        "the helper did crash on the second utterance"
    );
    assert!(finished, "{:?}", stream.status());
    assert_eq!(stream.status().phase, Phase::Complete);
    let saved = CoverageJournal::snapshot(sessions.path(), "fixture", Source::System).unwrap();
    let segments: Vec<_> = saved.commits.iter().flat_map(|c| &c.segments).collect();
    assert_eq!(
        segments.len(),
        3,
        "the crashed utterance was retried: {segments:?}"
    );
    assert!(saved.commits.iter().all(|c| c.gap.is_none()));
    assert_eq!(retained(sessions.path(), audio.len()), audio);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_utterance_that_keeps_crashing_the_helper_is_a_gap_and_later_speech_is_transcribed() {
    let (stream, finished, sessions, audio) = record(
        &crashing_launch(None),
        &[utterance(), poisoned(), utterance()],
    )
    .await;
    assert!(finished, "the leg keeps running: {:?}", stream.status());
    let status = stream.status();
    let note = status.error.as_deref().expect("the gap is reported");
    assert!(note.contains("not transcribed"), "{note}");
    let saved = CoverageJournal::snapshot(sessions.path(), "fixture", Source::System).unwrap();
    let segments: Vec<_> = saved.commits.iter().flat_map(|c| &c.segments).collect();
    assert_eq!(
        segments.len(),
        2,
        "first and third utterances: {segments:?}"
    );
    // Each utterance is 3 s on the recording clock, which starts at 137 ms;
    // a segment may start up to the endpointer's 300 ms preroll early.
    assert!(segments[0].start_ms < 137 + 3_000, "{segments:?}");
    assert!(segments[1].start_ms >= 137 + 6_000 - 300, "{segments:?}");
    let gaps: Vec<_> = saved.commits.iter().filter(|c| c.gap.is_some()).collect();
    assert_eq!(gaps.len(), 1, "{:?}", saved.commits);
    assert!(gaps[0].segments.is_empty());
    assert!(gaps[0].gap.as_deref().unwrap().contains("speech helper"));
    // The gap covers the poisoned utterance on the absolute sample clock.
    let poisoned_at = 137 * 16 + 48_000 + 16_000;
    assert!(gaps[0].from_sample <= poisoned_at && poisoned_at < gaps[0].through_sample);
    assert_eq!(
        retained(sessions.path(), audio.len()),
        audio,
        "the gap's audio is kept"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_helper_that_never_recovers_ends_transcription_promptly_and_keeps_the_audio() {
    let data_root = tempfile::tempdir().unwrap();
    // `fake-crash` loads like any engine and exits on every audio frame, so
    // the client's restart budget runs out.
    let client = fotw_speechd_client::SpeechdClient::start(
        fotw_speechd_client::SpeechdConfig::new(fake_helper(), data_root.path())
            .arg("--fake-engines"),
    )
    .await
    .unwrap();
    let engine = fotw_stt::SpeechdEngine::prepare(client, "fake-crash")
        .await
        .unwrap();
    let sessions = tempfile::tempdir().unwrap();
    // Each failed utterance is retried after a restart at least 2 s after the
    // last, so give the drain the production deadline.
    let mut config = config(std::sync::Arc::new(engine), "fake".into());
    config.stop_timeout = Duration::from_secs(30);
    let (stream, _) = FailoverStream::open_local(sessions.path(), config)
        .await
        .unwrap();
    let audio = [utterance(), utterance(), utterance()].concat();
    stream.write(&audio).unwrap();
    let started = Instant::now();
    let finished = tokio::time::timeout(Duration::from_secs(40), stream.finish())
        .await
        .expect("a dead helper never holds up the end of a recording");
    assert!(!finished);
    assert!(started.elapsed() < Duration::from_secs(30));
    let status = stream.status();
    assert_eq!(status.phase, Phase::Incomplete, "{status:?}");
    let error = status.error.as_ref().expect("the failure is reported");
    assert!(error.contains("speech helper"), "{error:?}");
    assert_eq!(
        retained(sessions.path(), audio.len()),
        audio,
        "the recording keeps every sample"
    );
}
