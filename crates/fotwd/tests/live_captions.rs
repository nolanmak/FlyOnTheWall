//! Live captions (#245) through a whole local session, with a scripted
//! recognizer for the final text and a scripted caption engine, so the run
//! needs no model, no helper and no network.
//!
//! What must hold: captions reach the live view as partials and nothing else;
//! no caption text is written anywhere in the session directory (the WAL,
//! journal and replay files that recovery reads after a crash); the final
//! transcript is the same with captions on or off; and a caption engine that
//! keeps failing pauses captions without touching recording or final text.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, SampleFormat, StreamFormat, TapId};
use fotw_stt::{Recognized, Recognizer, RecognizerFactory, SttError, TranscriptSegment};
use fotw_web::recorder::CaptionStatus;
use fotwd::captions::{CaptionEngine, CaptionEvent, CaptionSession, CaptionText, OpenFuture};
use fotwd::session::{self, LocalLegs, SegmentTap, SessionControl, TapKind, Transcription};
use tokio::sync::mpsc;

/// Text no final engine produces, to find caption text wherever it lands.
const MARKER: &str = "CAPTION-ONLY-7f3a";

fn tmpdir(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("fotwd-captions-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("sessions")).unwrap();
    base
}

/// Speech-like audio from the first frame (see tests/local_stt.rs).
fn tone(seconds: f32) -> WavData {
    let format = StreamFormat::new(48_000, 2, SampleFormat::I16);
    let n = (48_000.0 * seconds) as usize;
    let mut samples = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / 48_000.0;
        let level = if ((t / 0.12) as u32).is_multiple_of(2) {
            0.5
        } else {
            0.05
        };
        let v = (t * 440.0 * std::f32::consts::TAU).sin() * level;
        samples.push(v);
        samples.push(v);
    }
    WavData { format, samples }
}

struct Scripted;

impl Recognizer for Scripted {
    fn transcribe(&mut self, _pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        Ok(vec![Recognized {
            start_ms: 0,
            end_ms: 400,
            text: "heard locally".into(),
        }])
    }
}

struct Final;

impl RecognizerFactory for Final {
    fn model(&self) -> &str {
        "scripted"
    }
    fn language(&self) -> Option<&str> {
        None
    }
    fn create(&self) -> Result<Box<dyn Recognizer>, SttError> {
        Ok(Box::new(Scripted))
    }
}

/// A caption engine that captions every chunk with [`MARKER`], or whose
/// streams always fail.
struct Captions {
    fail: bool,
}

impl CaptionEngine for Captions {
    fn model(&self) -> String {
        "scripted-captions".into()
    }

    fn open(&self) -> OpenFuture<'_> {
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                return Err("scripted caption failure".into());
            }
            let (audio, mut pcm) = mpsc::channel::<Vec<i16>>(64);
            let (events, receiver) = mpsc::channel(256);
            tokio::spawn(async move {
                let mut samples = 0u64;
                while let Some(chunk) = pcm.recv().await {
                    samples += chunk.len() as u64;
                    let text = CaptionText {
                        utterance: 1,
                        text: format!("{MARKER} {samples}"),
                        start_ms: 0,
                        end_ms: samples / 16,
                    };
                    if events.send(CaptionEvent::Partial(text)).await.is_err() {
                        return;
                    }
                }
            });
            Ok(CaptionSession {
                audio,
                events: receiver,
            })
        })
    }
}

struct Run {
    outcome: session::SessionOutcome,
    tapped: Vec<(TranscriptSegment, TapKind)>,
    captions: Option<CaptionStatus>,
}

async fn record(name: &str, captions: Option<Captions>) -> Run {
    let root = tmpdir(name);
    let mut control = SessionControl::new();
    let tapped = Arc::new(Mutex::new(Vec::new()));
    let sink = tapped.clone();
    control.on_segment =
        SegmentTap::new(move |seg, kind| sink.lock().unwrap().push((seg.clone(), kind)));
    let speech = control.speech.clone();
    let stop = control.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        stop.stop();
    });
    let mut legs = LocalLegs::for_session(Arc::new(Final), "placeholder", false, true);
    legs.captions = captions.map(|c| Arc::new(c) as Arc<dyn CaptionEngine>);
    let tap: Box<dyn AudioTap> = Box::new(FileAudioSource::from_wav(
        TapId::system_default(),
        tone(30.0),
        ReplaySpeed::Realtime,
    ));
    let outcome = session::run_with_control(
        &root.join("sessions"),
        tap,
        None,
        Transcription::Local(legs),
        Duration::from_secs(30),
        control,
    )
    .await
    .expect("session runs");
    let captions = speech
        .snapshot()
        .into_iter()
        .find(|s| s.source == fotw_web::recorder::CaptureSource::System)
        .and_then(|s| s.captions);
    let tapped = tapped.lock().unwrap().clone();
    Run {
        outcome,
        tapped,
        captions,
    }
}

/// Every file under `dir`, recursively, as bytes.
fn files(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push((path, bytes));
        }
    }
    out
}

/// What the saved transcript says, without its random ids.
fn transcript(segments: &[TranscriptSegment]) -> String {
    let rows: Vec<_> = segments
        .iter()
        .map(|s| {
            serde_json::json!({
                "source": s.source, "text": s.text, "start_ms": s.start_ms, "end_ms": s.end_ms,
                "provider": s.provider, "model": s.model, "is_final": s.is_final,
            })
        })
        .collect();
    serde_json::to_string(&rows).unwrap()
}

#[tokio::test]
async fn captions_reach_the_live_view_and_are_never_written_anywhere() {
    let run = record("live", Some(Captions { fail: false })).await;

    let captions: Vec<_> = run
        .tapped
        .iter()
        .filter(|(s, _)| s.provider == fotwd::captions::PROVIDER)
        .collect();
    assert!(!captions.is_empty(), "captions reached the live view");
    assert!(
        captions
            .iter()
            .all(|(s, kind)| *kind == TapKind::Partial && !s.is_final),
        "only ever as partials"
    );
    assert!(captions.iter().all(|(s, _)| s.text.contains(MARKER)));

    // The saved transcript is the final engine's alone.
    assert!(!run.outcome.segments.is_empty());
    assert!(
        run.outcome
            .segments
            .iter()
            .all(|s| !s.text.contains(MARKER))
    );
    // And nothing on disk holds caption text, so a crash and restart has none
    // to recover: recovery reads only these files.
    for (path, bytes) in files(&run.outcome.dir) {
        assert!(
            !bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
            "caption text written to {}",
            path.display()
        );
    }
    assert_eq!(run.captions, Some(CaptionStatus::Live));
    assert!(
        run.outcome.stt_errors.is_empty(),
        "{:?}",
        run.outcome.stt_errors
    );
}

#[tokio::test]
async fn the_final_transcript_is_identical_with_captions_on_or_off() {
    let off = record("off", None).await;
    let on = record("on", Some(Captions { fail: false })).await;
    assert!(!off.outcome.segments.is_empty());
    assert_eq!(
        transcript(&on.outcome.segments),
        transcript(&off.outcome.segments)
    );
    assert_eq!(off.captions, None, "no caption state without captions");
}

#[tokio::test]
async fn a_failing_caption_engine_pauses_captions_and_nothing_else() {
    let run = record("failing", Some(Captions { fail: true })).await;
    assert_eq!(run.captions, Some(CaptionStatus::Paused));
    assert!(
        run.tapped
            .iter()
            .all(|(s, _)| s.provider != fotwd::captions::PROVIDER),
        "no captions while paused"
    );
    // Recording and final text carry on, and caption trouble is not a
    // transcription error.
    assert!(run.outcome.system_samples > 0);
    assert_eq!(run.outcome.segments.len(), 1, "{:?}", run.outcome.segments);
    assert_eq!(run.outcome.segments[0].text, "heard locally");
    assert!(
        run.outcome.stt_errors.is_empty(),
        "{:?}",
        run.outcome.stt_errors
    );
}
