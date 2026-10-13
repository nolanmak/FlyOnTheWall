//! Capture -> PCM -> mock provider -> live events -> independent stored meeting.
//! No provider credentials, real devices, or private library are used.
use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{AudioTap, CaptureTimestamp, FrameFlags, FrameSink, TapError};
use fotw_audio::{SampleFormat, StreamFormat, TapId};
use fotw_stt::deepgram_wire::DeepgramEndpoint;
use fotwd::session::{self, DeepgramLegs, SegmentTap, SessionControl, TapKind, Transcription};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

async fn provider(
    word: &'static str,
    speech_ms: u64,
) -> (DeepgramEndpoint, tokio::task::JoinHandle<usize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = DeepgramEndpoint::loopback(listener.local_addr().unwrap().port());
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        // A delayed handshake must not move the first word to connection time.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let mut samples = 0;
        let mut sent = false;
        while let Some(Ok(msg)) = socket.next().await {
            match msg {
                Message::Binary(bytes) => {
                    samples += bytes.len() / 2;
                    if !sent && samples >= (speech_ms as usize + 500) * 16 {
                        let start = speech_ms as f64 / 1000.0;
                        let frame = serde_json::json!({"type":"Results","channel_index":[0,1],"start":start,"duration":0.5,"is_final":true,"speech_final":true,"channel":{"alternatives":[{"transcript":word,"confidence":1.0,"words":[{"word":word,"start":start,"end":start+0.5,"confidence":1.0}]}]}});
                        socket
                            .send(Message::Text(frame.to_string().into()))
                            .await
                            .unwrap();
                        sent = true;
                    }
                }
                Message::Text(text) if text.contains("CloseStream") => {
                    let _ = socket.close(None).await;
                    break;
                }
                _ => {}
            }
        }
        assert!(sent, "the provider must receive the prefix and speech");
        samples
    });
    (endpoint, task)
}
fn source(id: TapId, speech_ms: u64) -> Box<dyn fotw_audio::AudioTap> {
    let samples = (0..48 * (speech_ms + 2000))
        .map(|i| {
            if i < 48 * speech_ms {
                0.0
            } else {
                ((i as f32) * 0.057).sin() * 0.2
            }
        })
        .collect();
    Box::new(FileAudioSource::from_wav(
        id,
        WavData {
            format: StreamFormat::new(48_000, 1, SampleFormat::F32),
            samples,
        },
        ReplaySpeed::Multiplier(20.0),
    ))
}
struct ObservedTap {
    inner: Box<dyn AudioTap>,
    first: Arc<AtomicU64>,
    delay: Duration,
}
struct ObservedSink {
    inner: Box<dyn FrameSink>,
    first: Arc<AtomicU64>,
}
impl FrameSink for ObservedSink {
    fn on_frames(&mut self, pcm: &[f32], ts: CaptureTimestamp, flags: FrameFlags) {
        let _ =
            self.first
                .compare_exchange(u64::MAX, ts.host_ns, Ordering::SeqCst, Ordering::SeqCst);
        self.inner.on_frames(pcm, ts, flags);
    }
    fn on_error(&mut self, e: TapError) {
        self.inner.on_error(e);
    }
}
impl AudioTap for ObservedTap {
    fn id(&self) -> &TapId {
        self.inner.id()
    }
    fn format(&self) -> StreamFormat {
        self.inner.format()
    }
    fn format_is_authoritative(&self) -> bool {
        self.inner.format_is_authoritative()
    }
    fn start(&mut self, sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        std::thread::sleep(self.delay);
        self.inner.start(Box::new(ObservedSink {
            inner: sink,
            first: self.first.clone(),
        }))
    }
    fn stop(&mut self) -> Result<(), TapError> {
        self.inner.stop()
    }
}
async fn check_capture(speech_ms: u64, mic_delay: Duration) {
    let root = tempfile::tempdir().unwrap();
    let (sys, sys_task) = provider("systemword", speech_ms).await;
    let (mic, mic_task) = provider("micword", speech_ms).await;
    let mut legs = DeepgramLegs::for_session("fake-key", "factory-placeholder", true, true);
    legs.system.endpoint = sys;
    legs.mic.as_mut().unwrap().endpoint = mic;
    let mut control = SessionControl::new();
    let ready = control.ready.clone();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = heard.clone();
    let stop = control.stop.clone();
    control.on_segment = SegmentTap::new(move |s, kind| {
        if matches!(kind, TapKind::Final) {
            let mut h = sink.lock().unwrap();
            h.push(s.clone());
            if h.len() == 2 {
                stop.stop();
            }
        }
    });
    let sys_first = Arc::new(AtomicU64::new(u64::MAX));
    let mic_first = Arc::new(AtomicU64::new(u64::MAX));
    let sys_tap = Box::new(ObservedTap {
        inner: source(TapId::system_default(), speech_ms),
        first: sys_first.clone(),
        delay: Duration::ZERO,
    });
    let mic_tap = Box::new(ObservedTap {
        inner: source(TapId::mic("fixture"), speech_ms),
        first: mic_first.clone(),
        delay: mic_delay,
    });
    let session_root = root.path().to_path_buf();
    let task = tokio::spawn(async move {
        session::run_with_control(
            &session_root,
            sys_tap,
            Some(mic_tap),
            Transcription::Deepgram(legs),
            Duration::from_secs(5),
            control,
        )
        .await
    });
    let mut db = fotw_store::Db::open(
        root.path().join("test.db"),
        &fotw_store::DbKey::from_bytes([3; 32]),
    )
    .unwrap();
    let old = db
        .meetings()
        .create(fotw_store::NewMeeting::new("test", "UTC"))
        .unwrap();
    let old_transcript = db
        .meetings()
        .create_transcript(&old, "fixture", "fixture", true)
        .unwrap();
    db.meetings()
        .append_segments(
            &old_transcript,
            &[fotw_store::NewSegment::new(
                0,
                5000,
                6000,
                "Earlier meeting only",
            )],
        )
        .unwrap();
    db.meetings().set_state(&old, "ready").unwrap();
    let old_before = db.export_meeting(&old).unwrap().to_json_pretty();
    // Reading a saved meeting while capture runs cannot select its destination.
    tokio::task::spawn_blocking(move || assert!(ready.wait_timeout(Duration::from_secs(3))))
        .await
        .unwrap();
    assert_eq!(
        db.export_meeting(&old).unwrap().to_json_pretty(),
        old_before
    );
    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(outcome.dropped_samples, 0);
    assert_eq!(outcome.segments.len(), 2, "{:?}", outcome.stt_errors);
    let id = outcome.dir.file_name().unwrap().to_str().unwrap();
    for seg in &outcome.segments {
        assert_eq!(seg.session_id, id);
        let system_ns = sys_first.load(Ordering::SeqCst);
        let mic_ns = mic_first.load(Ordering::SeqCst);
        let first = if seg.source == fotw_stt::Source::Mic {
            mic_ns
        } else {
            system_ns
        };
        assert_eq!(
            seg.start_ms,
            speech_ms + (first - system_ns.min(mic_ns)) / 1_000_000
        );
    }
    for seg in heard.lock().unwrap().iter() {
        assert_eq!(seg.session_id, id);
    }
    let pcm = std::fs::read(outcome.dir.join("system.pcm")).unwrap();
    let prefix = speech_ms as usize * 48 * 2;
    assert!(pcm.len() >= prefix);
    assert!(pcm[..prefix].iter().all(|b| *b == 0));
    assert!(pcm[prefix..].iter().any(|b| *b != 0));
    let new = fotwd::persist::persist_session(&mut db, &outcome, "New meeting").unwrap();
    assert_ne!(old, new);
    assert_eq!(
        db.export_meeting(&old).unwrap().to_json_pretty(),
        old_before
    );
    let doc = db.export_meeting(&new).unwrap();
    assert!(doc.to_plain_text().contains("[00:00:00]"));
    assert!(doc.segments.iter().all(|s| s.start_ms >= speech_ms as i64));
    sys_task.await.unwrap();
    mic_task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn leading_silence_keeps_canonical_offsets_and_persists_as_a_separate_meeting() {
    check_capture(17000, Duration::ZERO).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn speech_at_capture_zero_is_retained_despite_delayed_provider_connection() {
    check_capture(0, Duration::ZERO).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn staggered_file_taps_use_measured_host_anchors_end_to_end() {
    check_capture(17000, Duration::from_millis(30)).await;
}
