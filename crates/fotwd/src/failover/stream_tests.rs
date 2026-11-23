use super::{FailoverStream, LocalSourceConfig, Ordering, Phase, ReplayReader, ReplaySource};
use std::sync::Arc;

#[path = "../../tests/support/held_local.rs"]
mod held_local;
use held_local::{TIMEOUT, held_after_more_input};

/// Audio written while the model is busy, then Stop, then the model returns:
/// the worker must finish all of the input, not the end it saw before the
/// model started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_written_during_inference_before_stop_is_still_transcribed() {
    let dir = tempfile::tempdir().unwrap();
    let (stream, release, audio) = held_after_more_input(dir.path()).await;
    let stream = Arc::new(stream);
    let stopping = tokio::spawn({
        let stream = stream.clone();
        async move { stream.finish().await }
    });
    // The seal is written before this flag. Observing the flag ensures
    // the worker sees closed input on return from the held decode.
    tokio::time::timeout(TIMEOUT, async {
        while !stream.shared.input_closed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Stop never closed the input");
    let reader = ReplayReader::open(dir.path(), "fixture", ReplaySource::Mic).unwrap();
    assert_eq!(
        reader.sealed_end().unwrap(),
        Some(137 * 16 + audio.len() as u64)
    );
    drop(release);
    let complete = stopping.await.unwrap();
    let status = stream.status();
    assert!(complete, "{status:?}");
    assert_eq!(status.phase, Phase::Complete);
    assert_eq!(status.committed_sample, 137 * 16 + audio.len() as u64);
    assert_eq!(status.lag_samples, 0);
}
