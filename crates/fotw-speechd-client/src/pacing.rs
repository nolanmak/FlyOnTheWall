//! Pacing a diarization stream (#246).
//!
//! A diarization stream carries a whole channel of a meeting, up to its
//! engine's `max_audio_ms` (4 hours), but the helper fails any stream that
//! queues more than 2 minutes of audio ahead of its engine (`busy`). The
//! helper acknowledges every 10 s window it has processed with
//! `partial {text: "", start_ms: 0, end_ms: <ms processed so far>}`, so the
//! caller keeps its unacknowledged audio under a bound by waiting on those:
//!
//! ```no_run
//! # async fn run(client: &fotw_speechd_client::SpeechdClient, pcm: &[i16])
//! # -> Result<(), fotw_speechd_client::ClientError> {
//! use fotw_speechd_client::{DIARIZATION_MAX_UNACKED_MS, StreamConfig, StreamEvent};
//! let mut stream = client.open_stream(StreamConfig::new("diarizer", 16_000)).await?;
//! let mut sent_samples = 0u64;
//! let mut acked_ms = 0u64;
//! for window in pcm.chunks(160_000) {
//!     stream.send_audio(window).await?;
//!     sent_samples += window.len() as u64;
//!     let sent_ms = sent_samples * 1000 / 16_000;
//!     if sent_ms > acked_ms + DIARIZATION_MAX_UNACKED_MS {
//!         acked_ms = stream.await_progress(sent_ms - DIARIZATION_MAX_UNACKED_MS).await?;
//!     }
//! }
//! stream.finish().await?;
//! while let Some(event) = stream.next_event().await {
//!     match event {
//!         StreamEvent::Final(result) => { /* result.segments: start_ms, end_ms, speaker */ }
//!         StreamEvent::Closed(_) | StreamEvent::Failed(_) => break,
//!         StreamEvent::Partial { .. } | StreamEvent::Vad { .. } => {}
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! After `finish` the helper has at most the unacknowledged audio plus one
//! window left to process, so the client's 120 s no-result deadline after
//! `finish` holds with a wide margin.

use crate::client::{ClientError, SpeechStream, StreamEvent};

/// How far ahead of the helper's acknowledgements a diarization stream's
/// caller should let its audio run: well under the helper's 2-minute backlog,
/// and a multiple of its 10 s window, so the wait always ends.
pub const DIARIZATION_MAX_UNACKED_MS: u64 = 60_000;

impl SpeechStream {
    /// On a diarization stream, wait until the helper has acknowledged
    /// processing at least `at_least_ms` of audio, and return the `end_ms`
    /// of the acknowledgement that got there. Acknowledgements come per 10 s
    /// window, so ask only for audio at least one window old.
    ///
    /// # Errors
    /// The stream's failure as [`ClientError::Remote`] (its code, such as
    /// `too_long`, `busy`, `engine` or `helper_crashed`);
    /// [`ClientError::StreamClosed`] if the stream ended first;
    /// [`ClientError::Protocol`] for an event a diarization stream does not
    /// send before `finish`.
    pub async fn await_progress(&mut self, at_least_ms: u64) -> Result<u64, ClientError> {
        loop {
            match self.next_event().await {
                Some(StreamEvent::Partial { end_ms, .. }) if end_ms >= at_least_ms => {
                    return Ok(end_ms);
                }
                Some(StreamEvent::Partial { .. }) => {}
                Some(StreamEvent::Failed(failure)) => {
                    return Err(ClientError::Remote {
                        code: failure.code,
                        message: failure.message,
                    });
                }
                Some(StreamEvent::Closed(_)) | None => return Err(ClientError::StreamClosed),
                Some(other) => {
                    return Err(ClientError::Protocol(format!(
                        "expected a progress partial, got {other:?}"
                    )));
                }
            }
        }
    }
}
