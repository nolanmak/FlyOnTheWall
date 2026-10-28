//! Shared consumer contracts; synthetic audio and local workers only.
use crate::deepgram::{DeepgramConfig, DeepgramNormalizer};
use crate::{PcmRing, Source, SttErrorClass, TimestampSource, to_linear16_le};

/// Verify timing units, final/interim semantics, malformed input and replay bounds.
///
/// # Panics
/// If a shared normalization or framing contract changes.
pub fn normalization() {
    let mut normalizer = DeepgramNormalizer::new(DeepgramConfig::new("contract", Source::Mic));
    let mut wire = serde_json::json!({
        "type":"Results", "is_final":false, "speech_final":false,
        "channel":{"alternatives":[{"transcript":"hello", "confidence":0.9,
            "words":[{"word":"hello", "start":1.02, "end":1.18, "confidence":0.9}]}]}
    });
    let interim = normalizer.push_json(&wire.to_string()).unwrap().unwrap();
    assert!(!interim.is_final);
    wire["is_final"] = true.into();
    wire["speech_final"] = true.into();
    let final_segment = normalizer.push_json(&wire.to_string()).unwrap().unwrap();
    assert!(final_segment.is_final);
    assert_eq!(final_segment.session_id, "contract");
    assert_eq!(final_segment.start_ms, 1020);
    assert_eq!(final_segment.end_ms, 1180);
    assert_eq!(final_segment.timestamp_source, TimestampSource::Provider);
    assert_eq!(final_segment.text, "hello");
    // Meeting adapters explicitly finalize an open utterance at end-of-stream.
    // Murmur's separate transport still requires CloseStream/Metadata evidence.
    wire["is_final"] = false.into();
    wire["speech_final"] = false.into();
    let open = normalizer.push_json(&wire.to_string()).unwrap().unwrap();
    let finished = normalizer.finish().unwrap();
    assert!(finished.is_final);
    assert_eq!(finished.id, open.id);
    assert_eq!(finished.text, open.text);
    assert!(normalizer.finish().is_none());

    assert_eq!(
        normalizer.push_json("{").unwrap_err().class,
        SttErrorClass::Server
    );
    assert_eq!(
        to_linear16_le(&[-32768, 0, 32767]),
        [0, 128, 0, 0, 255, 127]
    );
    let mut ring = PcmRing::new(16_000, 100);
    for _ in 0..3 {
        ring.push(&[7; 1600]);
    }
    assert_eq!(ring.written_ms(), 300);
    assert_eq!(ring.buffered_ms(), 100);
    let replay = ring.replay_from(0);
    assert_eq!(replay.start_ms, 200);
    assert_eq!(replay.truncated_ms, 200);
    assert_eq!(replay.samples.len(), 1600);
}

use crate::{
    LocalStream, LocalStreamConfig, Recognized, Recognizer, RecognizerFactory, StreamEvent,
    SttError,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct FixtureFactory {
    calls: Arc<AtomicUsize>,
    gate: Option<Mutex<std::sync::mpsc::Receiver<()>>>,
}
struct FixtureRecognizer(Arc<AtomicUsize>);
impl Recognizer for FixtureRecognizer {
    fn transcribe(&mut self, _: &[f32]) -> Result<Vec<Recognized>, SttError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Recognized {
            start_ms: 0,
            end_ms: 500,
            text: "fixture words".into(),
        }])
    }
}
impl RecognizerFactory for FixtureFactory {
    fn model(&self) -> &str {
        "contract-fixture"
    }
    fn language(&self) -> Option<&str> {
        Some("en")
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        if let Some(gate) = &self.gate {
            gate.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        Ok(Box::new(FixtureRecognizer(self.calls.clone())))
    }
}
fn speech() -> Vec<i16> {
    (0..24_000)
        .map(|i| {
            let amp = if (i / 1920) % 2 == 0 { 8000 } else { 800 };
            if (i / 20) % 2 == 0 { amp } else { -amp }
        })
        .collect()
}
async fn events(mut rx: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
    tokio::time::timeout(Duration::from_secs(5), async move {
        let mut out = vec![];
        while let Some(event) = rx.recv().await {
            out.push(event);
        }
        out
    })
    .await
    .expect("fixture worker must terminate")
}

/// Verify local silence, session offsets, cancellation and backlog limits.
///
/// Uses a scripted recognizer; neither weights nor provider credentials are read.
/// # Panics
/// If output is lost, fabricated after cancellation, shifted or unbounded.
pub async fn local_lifecycle() {
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FixtureFactory {
        calls: calls.clone(),
        gate: None,
    });
    let (stream, rx) = LocalStream::open(
        LocalStreamConfig::new("silence", Source::Mic),
        factory.clone(),
    );
    stream.write(&vec![0; 16_000]);
    stream.close().await.unwrap();
    assert!(events(rx).await.iter().all(|e| e.final_segment().is_none()));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (stream, rx) = LocalStream::open(
        LocalStreamConfig::new("timed", Source::Mic).with_session_offset_ms(1000),
        factory,
    );
    stream.write(&vec![0; 15_840]);
    stream.write(&speech());
    stream.close().await.unwrap();
    let output = events(rx).await;
    let finals: Vec<_> = output
        .iter()
        .filter_map(StreamEvent::final_segment)
        .collect();
    assert_eq!(finals.len(), 1);
    assert_eq!(finals[0].session_id, "timed");
    assert_eq!(finals[0].start_ms, 1690);
    assert_eq!(finals[0].text, "fixture words");

    for overload in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, gate) = std::sync::mpsc::channel();
        let factory = Arc::new(FixtureFactory {
            calls: calls.clone(),
            gate: Some(Mutex::new(gate)),
        });
        let (stream, rx) =
            LocalStream::open(LocalStreamConfig::new("abandoned", Source::Mic), factory);
        if overload {
            stream.write(&vec![8000; 61 * 16_000]);
        } else {
            stream.write(&speech());
        }
        assert!(stream.queued_samples() <= 60 * 16_000);
        drop(stream);
        release.send(()).unwrap();
        let output = events(rx).await;
        assert!(output.iter().all(|e| e.final_segment().is_none()));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        if overload {
            assert!(output.iter().any(|e| matches!(e, StreamEvent::Error(_))));
        }
    }
}

/// Names, numbers, technical terms, Unicode, exact boundaries and literal snippets.
///
/// # Panics
/// If the shared vocabulary wire shape or deterministic semantics change.
pub fn vocabulary() {
    use crate::{
        CustomVocabulary,
        vocabulary::{HintSelection, Profile},
    };
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/vocabulary-v1.json")).unwrap();
    let profile = Profile::parse(&serde_json::to_vec(&fixture["profile"]).unwrap()).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        assert_eq!(
            profile.expand(case["input"].as_str().unwrap()),
            case["output"].as_str().unwrap()
        );
    }
    let roundtrip = Profile::parse(&serde_json::to_vec_pretty(&profile).unwrap()).unwrap();
    assert_eq!(roundtrip, profile);
    assert_eq!(
        profile.hints_for(CustomVocabulary::None),
        HintSelection::Unsupported
    );
    assert_eq!(
        profile.hints_for(CustomVocabulary::Keyterm),
        HintSelection::Supported {
            mechanism: CustomVocabulary::Keyterm,
            terms: &profile.recognition_hints
        }
    );
    assert_eq!(
        Profile::default().hints_for(CustomVocabulary::None),
        HintSelection::NotRequested
    );
    for mechanism in [
        CustomVocabulary::Keyterm,
        CustomVocabulary::Keywords,
        CustomVocabulary::Prompt,
    ] {
        assert_eq!(
            profile.hints_for(mechanism),
            HintSelection::Supported {
                mechanism,
                terms: &profile.recognition_hints
            }
        );
    }
    let mut snippets_only = profile.clone();
    snippets_only.recognition_hints.clear();
    assert_eq!(
        snippets_only.hints_for(CustomVocabulary::None),
        HintSelection::NotRequested
    );
    vocabulary_corrections();
}

/// Local corrections: exact, case, accents, near misses, phrases and guards.
///
/// # Panics
/// If local vocabulary correction semantics change.
pub fn vocabulary_corrections() {
    use crate::{vocabulary::Profile, vocabulary_matcher::VocabularyMatcher};
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/vocabulary-corrections-v1.json"
    ))
    .unwrap();
    for group in fixture["groups"].as_array().unwrap() {
        let profile = Profile::parse(&serde_json::to_vec(&group["profile"]).unwrap()).unwrap();
        let matcher = VocabularyMatcher::new(&profile);
        for case in group["cases"].as_array().unwrap() {
            assert_eq!(
                matcher.apply(case["input"].as_str().unwrap()),
                case["output"].as_str().unwrap(),
                "{}",
                case["why"]
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_normalization() {
        super::normalization();
    }
    #[tokio::test]
    async fn shared_local_lifecycle() {
        super::local_lifecycle().await;
    }
}
