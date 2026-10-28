//! Parakeet Unified through `fotw-speechd` (#244), against the model-free
//! `fotw-speechd-fake`: no model, no network, no Swift.
//!
//! What a meeting relies on: each utterance the endpointer cuts becomes a final
//! on the recording clock with the helper's model and revision on it; a missing
//! model is refused before capture with the command that installs it; and a
//! helper that dies costs that utterance, never the leg.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fotw_speechd_client::{SpeechdClient, SpeechdConfig};
use fotw_stt::speechd::{PARAKEET_UNIFIED, PARAKEET_UNIFIED_REVISION, PROVIDER};
use fotw_stt::{
    LocalStream, LocalStreamConfig, RecognizerFactory, Source, SpeechdEngine, StreamEvent,
    StreamState, SttErrorClass,
};

const RATE_PER_MS: usize = 16;

/// `target/<profile>/fotw-speechd-fake`, built on first use if this test
/// binary was built on its own (`cargo test -p fotw-stt`).
fn fake_helper() -> PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let profile_dir = exe.parent().unwrap().parent().unwrap();
        let path = profile_dir.join("fotw-speechd-fake");
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
        assert!(path.is_file(), "no fake helper at {}", path.display());
        path
    })
    .clone()
}

/// A fake helper serving `parakeet-unified` at the pinned revision, installed
/// under `models_root` or not.
async fn client(models_root: &Path, installed: bool) -> SpeechdClient {
    if installed {
        std::fs::create_dir_all(
            models_root
                .join(PARAKEET_UNIFIED)
                .join(PARAKEET_UNIFIED_REVISION),
        )
        .unwrap();
    }
    let config = SpeechdConfig::new(fake_helper(), models_root)
        .arg("--fake-engines")
        .arg("--pinned")
        .arg(format!("{PARAKEET_UNIFIED}@{PARAKEET_UNIFIED_REVISION}"));
    SpeechdClient::start(config).await.unwrap()
}

/// A square wave whose level swings 20 dB every 120 ms, as syllables do.
fn speech(ms: usize) -> Vec<i16> {
    (0..ms * RATE_PER_MS)
        .map(|i| {
            let amp = if (i / 1_920) % 2 == 0 { 8_000 } else { 800 };
            if (i / 20) % 2 == 0 { amp } else { -amp }
        })
        .collect()
}

fn silence(ms: usize) -> Vec<i16> {
    vec![0; ms * RATE_PER_MS]
}

fn feed(stream: &LocalStream, pcm: &[i16]) {
    for piece in pcm.chunks(320) {
        stream.write(piece);
    }
}

async fn drain(events: &mut tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
    let mut seen = Vec::new();
    while let Some(event) = events.recv().await {
        let closed = matches!(event, StreamEvent::State(StreamState::Closed));
        seen.push(event);
        if closed {
            break;
        }
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_utterance_becomes_a_final_on_the_recording_clock_with_parakeet_provenance() {
    let root = tempfile::tempdir().unwrap();
    let engine = SpeechdEngine::prepare(client(root.path(), true).await, PARAKEET_UNIFIED)
        .await
        .unwrap();
    assert_eq!(engine.provider(), PROVIDER);
    let provenance = format!("{}@{}", engine.model(), engine.revision());
    assert_eq!(
        provenance,
        "parakeet-unified@d32e972dd4315f1dc3f6be28fb2aab0ab3e80358"
    );

    let config = LocalStreamConfig::new("s1", Source::System).with_session_offset_ms(5_000);
    let (stream, mut events) = LocalStream::open(config, std::sync::Arc::new(engine));
    // 990 ms is a whole number of the endpointer's 30 ms frames.
    feed(&stream, &silence(990));
    feed(&stream, &speech(2_000));
    feed(&stream, &silence(1_500));
    stream.close().await.unwrap();

    let finals: Vec<_> = drain(&mut events)
        .await
        .into_iter()
        .filter_map(|e| match e {
            StreamEvent::Final(segment) => Some(segment),
            StreamEvent::Error(error) => panic!("unexpected error: {error:?}"),
            _ => None,
        })
        .collect();
    assert_eq!(finals.len(), 1, "{finals:?}");
    let segment = &finals[0];
    assert_eq!(segment.provider, PROVIDER);
    assert_eq!(segment.model, PARAKEET_UNIFIED);
    // The utterance starts at the speech, less the endpointer's 300 ms preroll,
    // on the session clock; the helper timed it from the stream's first sample.
    assert_eq!(segment.start_ms, 5_000 + 990 - 300);
    assert!(segment.end_ms > segment.start_ms + 2_000, "{segment:?}");
    let samples: u64 = segment
        .text
        .strip_prefix("samples=")
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("the helper's text, not ours: {:?}", segment.text));
    assert_eq!(
        samples,
        (segment.end_ms - segment.start_ms) * 16,
        "every sample of the utterance reached the helper"
    );
    assert_eq!(segment.language.as_deref(), Some("en"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_model_is_refused_with_the_command_that_installs_it() {
    let root = tempfile::tempdir().unwrap();
    let refused = SpeechdEngine::prepare(client(root.path(), false).await, PARAKEET_UNIFIED)
        .await
        .err()
        .expect("no model, no engine");
    assert_eq!(refused.class, SttErrorClass::Unsupported);
    assert!(!refused.retryable);
    assert!(
        refused
            .message
            .contains("fotwd stt models install parakeet-unified"),
        "{}",
        refused.message
    );
    assert!(
        refused.message.contains("Transcription settings"),
        "{}",
        refused.message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_helper_crash_fails_that_utterance_and_never_hangs_the_leg() {
    let root = tempfile::tempdir().unwrap();
    // `fake-crash` exits on its first audio frame.
    let engine = SpeechdEngine::prepare(client(root.path(), true).await, "fake-crash")
        .await
        .unwrap();
    let (stream, mut events) = LocalStream::open(
        LocalStreamConfig::new("s1", Source::Mic),
        std::sync::Arc::new(engine),
    );
    let started = Instant::now();
    feed(&stream, &speech(1_000));
    feed(&stream, &silence(1_000));
    tokio::time::timeout(Duration::from_secs(10), stream.close())
        .await
        .expect("close never waits on a dead helper")
        .unwrap();
    let seen = drain(&mut events).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    let errors: Vec<_> = seen
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Error(error) => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "{seen:?}");
    assert_eq!(errors[0].provider, PROVIDER);
    assert!(
        errors[0].message.contains("speech helper"),
        "{}",
        errors[0].message
    );
    assert!(
        !seen.iter().any(|e| matches!(e, StreamEvent::Final(_))),
        "nothing is invented for the lost utterance"
    );
}
