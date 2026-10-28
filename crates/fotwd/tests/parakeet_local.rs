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

/// `target/<profile>/fotw-speechd-fake`, built on first use if needed.
fn fake_helper() -> PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let path = exe
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("fotw-speechd-fake");
        if !path.is_file() {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args([
                    "build",
                    "-p",
                    "fotw-speechd-client",
                    "--bin",
                    "fotw-speechd-fake",
                ])
                .status()
                .unwrap();
            assert!(status.success(), "could not build fotw-speechd-fake");
        }
        path
    })
    .clone()
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_helper_crash_ends_transcription_promptly_and_keeps_the_audio() {
    let data_root = tempfile::tempdir().unwrap();
    // `fake-crash` loads like any engine and exits on its first audio frame.
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
    let (stream, _) = FailoverStream::open_local(
        sessions.path(),
        config(std::sync::Arc::new(engine), "fake".into()),
    )
    .await
    .unwrap();
    let audio = utterance();
    stream.write(&audio).unwrap();
    let started = Instant::now();
    let finished = tokio::time::timeout(Duration::from_secs(10), stream.finish())
        .await
        .expect("a dead helper never holds up the end of a recording");
    assert!(!finished);
    assert!(started.elapsed() < Duration::from_secs(10));
    let status = stream.status();
    assert_eq!(status.phase, Phase::Incomplete, "{status:?}");
    let error = status.error.as_ref().expect("the failure is reported");
    assert!(error.contains("speech helper"), "{error:?}");

    let mut reader = ReplayReader::open(sessions.path(), "fixture", ReplaySource::System).unwrap();
    let mut retained = Vec::new();
    while retained.len() < audio.len() {
        let next = reader
            .read_from(137 * 16 + retained.len() as u64, 16_000)
            .unwrap();
        if next.is_empty() {
            break;
        }
        retained.extend(next);
    }
    assert_eq!(retained, audio, "the recording keeps every sample");
}
