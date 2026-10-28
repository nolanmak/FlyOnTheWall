//! Live captions (#245): word-by-word text while people talk, in local mode.
//!
//! A caption engine (Parakeet realtime EOU in the speech helper) runs beside
//! the final engine on each capture leg. Its text is **display only**: each
//! caption goes to the leg's event channel as a [`StreamEvent::Partial`], which
//! the collector hands to the live tap and never to storage, and the final
//! engine's segment text replaces it in the dashboard. Nothing a caption engine
//! says reaches the library, the WAL, an export or a summary.
//!
//! The pacing follows Muesli's `MeetingStreamingPartialSession.swift`
//! (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav Hari, commit
//! 97e455794112126a896cae17f5c829e8ffa820c8): audio is offered without ever
//! blocking capture, a caption engine that keeps failing goes dormant for the
//! session ("Live captions paused") instead of retrying forever, and final
//! text does not depend on it.
//!
//! The engine is behind [`CaptionEngine`], so everything here is tested with
//! a scripted engine and no helper; [`SpeechdCaptions`] is the helper-backed
//! one.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fotw_stt::{Source, StreamEvent, TimestampSource, TranscriptSegment};
use tokio::sync::mpsc;

/// Provider stamped on caption segments. `serve` keys pending captions in the
/// dashboard by segment id when it sees this.
pub const PROVIDER: &str = "live-captions";

/// The helper engine for captions.
pub const ENGINE: &str = "parakeet-eou-320ms";

/// Consecutive caption-engine errors after which captions sleep for the
/// session. A success in between resets the count.
pub const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// 16 kHz: samples per millisecond on a leg's clock.
const SAMPLES_PER_MS: u64 = 16;

/// Audio chunks queued for the caption task before new ones are dropped.
/// Dropping is the point: a slow caption engine must never slow capture or
/// the final engine. A gap restarts the caption stream on the new position.
const FEED_QUEUE: usize = 64;

/// Caption text from an engine, on its stream's own clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionText {
    /// Utterance number within the stream.
    pub utterance: u64,
    /// The text so far, or the utterance's committed text.
    pub text: String,
    /// Start, ms from the stream's first sample.
    pub start_ms: u64,
    /// End, ms from the stream's first sample.
    pub end_ms: u64,
}

/// What a caption stream yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptionEvent {
    /// Text of the utterance in progress.
    Partial(CaptionText),
    /// The utterance's last caption. Still display-only.
    Final(CaptionText),
    /// The stream failed and yields nothing more.
    Failed(String),
}

/// One open caption stream. Dropping `audio` finishes it.
pub struct CaptionSession {
    /// 16 kHz mono PCM for the engine, in order.
    pub audio: mpsc::Sender<Vec<i16>>,
    /// Captions, then the channel closes.
    pub events: mpsc::Receiver<CaptionEvent>,
}

/// Future returned by [`CaptionEngine::open`].
pub type OpenFuture<'a> = Pin<Box<dyn Future<Output = Result<CaptionSession, String>> + Send + 'a>>;

/// Something that can open caption streams.
pub trait CaptionEngine: Send + Sync + 'static {
    /// Open one stream.
    fn open(&self) -> OpenFuture<'_>;
    /// Model identity for caption segments.
    fn model(&self) -> String;
}

/// Captions from the speech helper's `parakeet-eou-320ms` engine.
pub struct SpeechdCaptions {
    client: fotw_speechd_client::SpeechdClient,
    engine: String,
    /// Loaded and warmed once, by the first stream to open, so a recording's
    /// Start never waits for the caption model: until it is ready the caption
    /// task simply drops audio. The helper's client replays the load after a
    /// helper restart.
    ready: tokio::sync::OnceCell<()>,
}

impl SpeechdCaptions {
    /// Captions on `client` (the daemon's shared helper) with `engine`.
    #[must_use]
    pub fn new(client: fotw_speechd_client::SpeechdClient, engine: impl Into<String>) -> Self {
        Self {
            client,
            engine: engine.into(),
            ready: tokio::sync::OnceCell::new(),
        }
    }
}

impl CaptionEngine for SpeechdCaptions {
    fn model(&self) -> String {
        self.engine.clone()
    }

    fn open(&self) -> OpenFuture<'_> {
        Box::pin(async move {
            use fotw_speechd_client::{StreamConfig, StreamEvent as Helper};
            self.ready
                .get_or_try_init(|| async {
                    self.client
                        .load(&self.engine)
                        .await
                        .map_err(|e| e.to_string())?;
                    self.client
                        .warmup(&self.engine)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                })
                .await?;
            let mut stream = self
                .client
                .open_stream(StreamConfig::new(&self.engine, 16_000))
                .await
                .map_err(|e| e.to_string())?;
            let (audio, mut pcm) = mpsc::channel::<Vec<i16>>(FEED_QUEUE);
            let (events, receiver) = mpsc::channel(256);
            tokio::spawn(async move {
                let text = |text, start_ms, end_ms, utterance: Option<u64>| CaptionText {
                    utterance: utterance.unwrap_or(1),
                    text,
                    start_ms,
                    end_ms,
                };
                let mut finishing = false;
                loop {
                    tokio::select! {
                        chunk = pcm.recv(), if !finishing => match chunk {
                            Some(chunk) => {
                                if let Err(e) = stream.send_audio(&chunk).await {
                                    let _ = events.send(CaptionEvent::Failed(e.to_string())).await;
                                    return;
                                }
                            }
                            None => {
                                finishing = true;
                                if stream.finish().await.is_err() {
                                    return;
                                }
                            }
                        },
                        event = stream.next_event() => {
                            let event = match event {
                                Some(Helper::Partial { text: t, start_ms, end_ms, utterance }) => {
                                    CaptionEvent::Partial(text(t, start_ms, end_ms, utterance))
                                }
                                Some(Helper::Final(f)) => {
                                    CaptionEvent::Final(text(f.text, f.start_ms, f.end_ms, f.utterance))
                                }
                                Some(Helper::Failed(failure)) => CaptionEvent::Failed(failure.message),
                                Some(Helper::Vad { .. }) => continue,
                                Some(Helper::Closed(_)) | None => return,
                            };
                            let failed = matches!(event, CaptionEvent::Failed(_));
                            if events.send(event).await.is_err() || failed {
                                return;
                            }
                        }
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

/// Where captions stand for one leg, for the recording status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionState {
    /// Captions are running.
    Live,
    /// Too many caption-engine errors: no captions for the rest of the
    /// session. Recording and final text carry on.
    Paused,
}

/// Reports a leg's caption state.
pub type StateFn = Arc<dyn Fn(Source, CaptionState) + Send + Sync>;

/// A leg's caption input. Offering audio never blocks.
pub struct CaptionFeed {
    tx: mpsc::Sender<(u64, Vec<i16>)>,
    position: AtomicU64,
}

impl CaptionFeed {
    /// Offer PCM the leg's final engine was just handed. Dropped, not waited
    /// for, when the caption task is behind or gone.
    pub fn offer(&self, pcm: &[i16]) {
        if pcm.is_empty() {
            return;
        }
        let start = self.position.fetch_add(pcm.len() as u64, Ordering::Relaxed);
        let _ = self.tx.try_send((start, pcm.to_vec()));
    }
}

/// What a leg's caption task needs.
pub struct CaptionLeg {
    /// The engine.
    pub engine: Arc<dyn CaptionEngine>,
    /// The leg.
    pub source: Source,
    /// The recording's session id, for segment identity.
    pub session_id: String,
    /// The leg's audio zero on the session clock, as for its final segments.
    pub anchor_ms: u64,
    /// The leg's event channel: captions go in as partials, beside the final
    /// engine's events, and so take the collector's display-only path.
    pub events: mpsc::UnboundedSender<StreamEvent>,
    /// Where to report this leg's caption state.
    pub state: StateFn,
}

/// Start a leg's caption task. Returns the feed the leg offers its PCM to.
#[must_use]
pub fn start(leg: CaptionLeg) -> CaptionFeed {
    let (tx, rx) = mpsc::channel(FEED_QUEUE);
    tokio::spawn(run(leg, rx));
    CaptionFeed {
        tx,
        position: AtomicU64::new(0),
    }
}

/// The open caption stream: its session, the leg sample it started at, and
/// its number within the recording (for segment ids).
struct Open {
    session: CaptionSession,
    base: u64,
    generation: u64,
}

async fn run(leg: CaptionLeg, mut audio: mpsc::Receiver<(u64, Vec<i16>)>) {
    let model = leg.engine.model();
    let mut open: Option<Open> = None;
    let mut next_sample = 0u64;
    let mut generation = 0u64;
    let mut errors = 0u32;
    (leg.state)(leg.source, CaptionState::Live);
    loop {
        tokio::select! {
            chunk = audio.recv() => {
                // The leg ended: let the open stream commit what it has.
                let Some((start, pcm)) = chunk else { break };
                let len = pcm.len() as u64;
                // A gap (dropped chunks) or no stream: start one here, so its
                // clock lines up with the leg's again.
                if open.is_none() || start != next_sample {
                    open = None;
                    generation += 1;
                    match leg.engine.open().await {
                        Ok(session) => open = Some(Open { session, base: start, generation }),
                        Err(_) => {
                            errors += 1;
                            if errors >= MAX_CONSECUTIVE_ERRORS {
                                break;
                            }
                        }
                    }
                }
                next_sample = start + len;
                if let Some(current) = &open
                    && current.session.audio.try_send(pcm).is_err()
                {
                    // Behind: restart at the next chunk rather than drift.
                    next_sample = u64::MAX;
                }
            }
            // Branch futures are dropped before a handler runs, so the
            // handlers above may replace `open`.
            event = async {
                match open.as_mut() {
                    Some(current) => current.session.events.recv().await,
                    None => std::future::pending().await,
                }
            } => match event {
                Some(CaptionEvent::Partial(text)) | Some(CaptionEvent::Final(text)) => {
                    errors = 0;
                    if let Some(current) = &open {
                        let segment = caption_segment(&leg, &model, current, text);
                        if leg.events.send(StreamEvent::Partial(segment)).is_err() {
                            return;
                        }
                    }
                }
                Some(CaptionEvent::Failed(_)) => {
                    open = None;
                    errors += 1;
                    if errors >= MAX_CONSECUTIVE_ERRORS {
                        break;
                    }
                }
                None => open = None,
            }
        }
    }
    if errors >= MAX_CONSECUTIVE_ERRORS {
        (leg.state)(leg.source, CaptionState::Paused);
        // Dormant: drain the feed so offers keep succeeding cheaply, without
        // touching the engine again this session.
        while audio.recv().await.is_some() {}
        return;
    }
    // Leg ended normally: finish the stream and pass on its last captions.
    if let Some(mut current) = open.take() {
        let (closed, _) = mpsc::channel(1);
        current.session.audio = closed;
        while let Some(event) = current.session.events.recv().await {
            if let CaptionEvent::Partial(text) | CaptionEvent::Final(text) = event {
                let segment = caption_segment(&leg, &model, &current, text);
                if leg.events.send(StreamEvent::Partial(segment)).is_err() {
                    break;
                }
            }
        }
    }
}

fn caption_segment(
    leg: &CaptionLeg,
    model: &str,
    open: &Open,
    text: CaptionText,
) -> TranscriptSegment {
    let base_ms = leg.anchor_ms + open.base / SAMPLES_PER_MS;
    let source = match leg.source {
        Source::System => "system",
        Source::Mic => "mic",
    };
    TranscriptSegment {
        // Stable per utterance: a later caption for the same utterance
        // replaces this one in the dashboard.
        id: format!("caption-{source}-{}-{}", open.generation, text.utterance),
        session_id: leg.session_id.clone(),
        source: leg.source,
        speaker: None,
        text: text.text,
        start_ms: base_ms + text.start_ms,
        end_ms: base_ms + text.end_ms,
        words: Vec::new(),
        confidence: None,
        language: None,
        is_final: false,
        revision: 0,
        provider: PROVIDER.to_owned(),
        model: model.to_owned(),
        timestamp_source: TimestampSource::Estimated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A caption engine scripted per opened stream: each stream echoes a
    /// partial per audio chunk ("<n> samples"), or fails on open or on its
    /// first chunk.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Script {
        Echo,
        FailOnOpen,
        FailOnAudio,
    }

    struct Scripted {
        scripts: Mutex<Vec<Script>>,
        opened: AtomicU64,
    }

    impl Scripted {
        fn new(scripts: &[Script]) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.iter().rev().copied().collect()),
                opened: AtomicU64::new(0),
            })
        }
    }

    impl CaptionEngine for Scripted {
        fn model(&self) -> String {
            "scripted".into()
        }

        fn open(&self) -> OpenFuture<'_> {
            Box::pin(async move {
                self.opened.fetch_add(1, Ordering::SeqCst);
                let script = self.scripts.lock().unwrap().pop().unwrap_or(Script::Echo);
                if script == Script::FailOnOpen {
                    return Err("scripted open failure".into());
                }
                let (audio, mut pcm) = mpsc::channel::<Vec<i16>>(8);
                let (events, receiver) = mpsc::channel(64);
                tokio::spawn(async move {
                    let mut samples = 0u64;
                    while let Some(chunk) = pcm.recv().await {
                        if script == Script::FailOnAudio {
                            let _ = events.send(CaptionEvent::Failed("scripted".into())).await;
                            return;
                        }
                        let start = samples;
                        samples += chunk.len() as u64;
                        let text = CaptionText {
                            utterance: 1,
                            text: format!("{samples} samples"),
                            start_ms: start / SAMPLES_PER_MS,
                            end_ms: samples / SAMPLES_PER_MS,
                        };
                        let _ = events.send(CaptionEvent::Partial(text)).await;
                    }
                    let text = CaptionText {
                        utterance: 1,
                        text: format!("{samples} samples, final"),
                        start_ms: 0,
                        end_ms: samples / SAMPLES_PER_MS,
                    };
                    let _ = events.send(CaptionEvent::Final(text)).await;
                });
                Ok(CaptionSession {
                    audio,
                    events: receiver,
                })
            })
        }
    }

    type States = Arc<Mutex<Vec<CaptionState>>>;

    fn leg(
        engine: Arc<dyn CaptionEngine>,
    ) -> (CaptionLeg, mpsc::UnboundedReceiver<StreamEvent>, States) {
        let (events, receiver) = mpsc::unbounded_channel();
        let states: States = Arc::default();
        let seen = states.clone();
        let leg = CaptionLeg {
            engine,
            source: Source::Mic,
            session_id: "s1".into(),
            anchor_ms: 1_000,
            events,
            state: Arc::new(move |_, state| seen.lock().unwrap().push(state)),
        };
        (leg, receiver, states)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> TranscriptSegment {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(StreamEvent::Partial(segment))) => segment,
            other => panic!("expected a caption, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn captions_are_partials_on_the_session_clock_and_never_finals() {
        let (leg, mut rx, states) = leg(Scripted::new(&[Script::Echo]));
        let feed = start(leg);
        feed.offer(&[1; 160]);
        feed.offer(&[1; 320]);
        let first = next(&mut rx).await;
        assert_eq!(
            (first.start_ms, first.end_ms),
            (1_000, 1_010),
            "leg anchor + stream time"
        );
        assert!(!first.is_final);
        assert_eq!(first.provider, PROVIDER);
        let second = next(&mut rx).await;
        assert_eq!(second.text, "480 samples");
        assert_eq!(
            second.id, first.id,
            "same utterance, same id: replaced in place"
        );
        drop(feed);
        let last = next(&mut rx).await;
        assert_eq!(last.text, "480 samples, final");
        assert!(
            !last.is_final,
            "an engine's final caption is still display-only"
        );
        assert_eq!(*states.lock().unwrap(), vec![CaptionState::Live]);
    }

    #[tokio::test]
    async fn three_consecutive_errors_put_captions_to_sleep_for_the_session() {
        let engine = Scripted::new(&[Script::FailOnOpen, Script::FailOnAudio, Script::FailOnOpen]);
        let (leg, mut rx, states) = leg(engine.clone());
        let feed = start(leg);
        for _ in 0..20 {
            feed.offer(&[1; 160]);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            *states.lock().unwrap(),
            vec![CaptionState::Live, CaptionState::Paused]
        );
        assert_eq!(
            engine.opened.load(Ordering::SeqCst),
            3,
            "no retry after the third error"
        );
        assert!(rx.try_recv().is_err(), "no captions once paused");
        // Offering keeps working, and costs nothing, while paused.
        feed.offer(&[1; 160]);
    }

    #[tokio::test]
    async fn a_success_between_errors_resets_the_count() {
        let engine = Scripted::new(&[Script::FailOnOpen, Script::FailOnOpen, Script::Echo]);
        let (leg, mut rx, states) = leg(engine);
        let feed = start(leg);
        for _ in 0..3 {
            feed.offer(&[1; 160]);
        }
        let caption = next(&mut rx).await;
        assert!(caption.text.ends_with("samples"));
        assert_eq!(*states.lock().unwrap(), vec![CaptionState::Live]);
    }

    #[tokio::test]
    async fn a_gap_in_the_feed_restarts_the_stream_on_the_leg_clock() {
        let engine = Scripted::new(&[Script::Echo, Script::Echo]);
        let (leg, mut rx, _) = leg(engine.clone());
        let feed = start(leg);
        feed.offer(&[1; 160]);
        let first = next(&mut rx).await;
        // Chunks the caption task never sees, as when its queue was full.
        feed.position.fetch_add(16_000, Ordering::Relaxed);
        feed.offer(&[1; 160]);
        let after = next(&mut rx).await;
        assert_eq!(
            engine.opened.load(Ordering::SeqCst),
            2,
            "a fresh stream after the gap"
        );
        assert_eq!(
            after.start_ms,
            1_000 + (160 + 16_000) / 16,
            "timed from where it restarted"
        );
        assert_ne!(
            after.id, first.id,
            "a new stream's utterances are new captions"
        );
    }
}
