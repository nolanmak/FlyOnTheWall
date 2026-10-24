//! The on-device stream's worker, driven with a scripted recognizer (#33).
//!
//! What the session relies on: finals arrive on the session clock, `close`
//! returns only after everything written has been transcribed, and a model
//! that will not load costs the transcript, never the recording.

use std::sync::{Arc, Mutex};

use fotw_stt::local::PROVIDER;
use fotw_stt::{
    LocalStream, LocalStreamConfig, Recognized, Recognizer, RecognizerFactory, Source, StreamEvent,
    StreamState, SttError, SttErrorClass,
};

const RATE_PER_MS: usize = 16;

/// Names each chunk it is given, in order, and remembers how long each was.
struct Scripted {
    heard: Arc<Mutex<Vec<usize>>>,
}

impl Recognizer for Scripted {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        let mut heard = self.heard.lock().unwrap();
        heard.push(pcm.len());
        Ok(vec![Recognized {
            start_ms: 0,
            end_ms: 500,
            text: format!("utterance {}", heard.len()),
        }])
    }
}

struct Factory {
    heard: Arc<Mutex<Vec<usize>>>,
    fail: bool,
}

impl RecognizerFactory for Factory {
    fn model(&self) -> &str {
        "scripted"
    }

    fn language(&self) -> Option<&str> {
        None
    }

    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        if self.fail {
            return Err(SttError::new(
                SttErrorClass::Unsupported,
                PROVIDER,
                "the local transcription model is missing",
            ));
        }
        Ok(Box::new(Scripted {
            heard: Arc::clone(&self.heard),
        }))
    }
}

/// A square wave whose level swings 20 dB every 120 ms, as syllables do: the
/// endpointer drops a steady tone as noise.
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

/// Write in the pump's 20 ms pieces.
fn feed(stream: &LocalStream, pcm: &[i16]) {
    for piece in pcm.chunks(320) {
        stream.write(piece);
    }
}

async fn drain(mut rx: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn finals_arrive_on_the_session_clock_and_close_waits_for_them() {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let engine = Arc::new(Factory {
        heard: Arc::clone(&heard),
        fail: false,
    });
    let config = LocalStreamConfig::new("s1", Source::Mic).with_session_offset_ms(1_000);
    let (stream, rx) = LocalStream::open(config, engine);

    feed(&stream, &silence(990));
    feed(&stream, &speech(1_500));
    feed(&stream, &silence(990));
    // Still mid-utterance when the meeting stops: close must transcribe it.
    feed(&stream, &speech(600));
    stream.close().await.unwrap();
    assert!(stream.is_closed());

    let events = drain(rx).await;
    let finals: Vec<_> = events
        .iter()
        .filter_map(StreamEvent::final_segment)
        .collect();
    assert_eq!(finals.len(), 2, "{events:?}");
    assert_eq!(finals[0].text, "utterance 1");
    // Leg offset 1000 ms + preroll-adjusted start at 690 ms.
    assert_eq!(finals[0].start_ms, 1_690);
    assert_eq!(finals[0].speaker.as_deref(), Some("me"));
    assert_eq!(finals[0].provider, PROVIDER);
    assert_eq!(finals[0].model, "scripted");
    assert_eq!(finals[1].text, "utterance 2");
    assert!(finals[1].start_ms > finals[0].end_ms);
    assert_eq!(
        events.first(),
        Some(&StreamEvent::State(StreamState::Connecting))
    );
    assert_eq!(
        events.last(),
        Some(&StreamEvent::State(StreamState::Closed))
    );
    assert_eq!(heard.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn flush_transcribes_the_open_utterance_without_closing() {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let engine = Arc::new(Factory {
        heard: Arc::clone(&heard),
        fail: false,
    });
    let (stream, rx) = LocalStream::open(LocalStreamConfig::new("s1", Source::System), engine);

    feed(&stream, &speech(1_000));
    stream.flush().await.unwrap();
    assert_eq!(heard.lock().unwrap().len(), 1);
    assert!(!stream.is_closed());

    stream.close().await.unwrap();
    // Idempotent, as the session's shutdown path may call it again.
    stream.close().await.unwrap();
    let finals = drain(rx)
        .await
        .into_iter()
        .filter(|e| e.final_segment().is_some())
        .count();
    assert_eq!(finals, 1);
}

#[tokio::test]
async fn a_model_that_will_not_load_reports_once_and_closes_cleanly() {
    let engine = Arc::new(Factory {
        heard: Arc::new(Mutex::new(Vec::new())),
        fail: true,
    });
    let (stream, rx) = LocalStream::open(LocalStreamConfig::new("s1", Source::System), engine);

    feed(&stream, &speech(2_000));
    feed(&stream, &silence(1_000));
    stream.close().await.unwrap();

    let events = drain(rx).await;
    let errors: Vec<_> = events.iter().filter_map(StreamEvent::error).collect();
    assert_eq!(errors.len(), 1, "{events:?}");
    assert!(errors[0].message.contains("missing"));
    assert!(events.iter().all(|e| e.final_segment().is_none()));
    assert_eq!(
        events.last(),
        Some(&StreamEvent::State(StreamState::Closed))
    );
}

#[tokio::test]
async fn writes_after_close_are_dropped_not_queued() {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let engine = Arc::new(Factory {
        heard: Arc::clone(&heard),
        fail: false,
    });
    let (stream, rx) = LocalStream::open(LocalStreamConfig::new("s1", Source::System), engine);
    stream.close().await.unwrap();
    feed(&stream, &speech(2_000));
    let _ = drain(rx).await;
    assert!(heard.lock().unwrap().is_empty());
}
