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
use std::time::Duration;

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

/// Why a caption stream did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// Every engine instance is in use for now (a stream that just ended is
    /// still handing its instance back). Retried, never counted as a failure.
    Busy(String),
    /// The engine failed. Counts toward pausing captions.
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

/// Future returned by [`CaptionEngine::open`].
pub type OpenFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CaptionSession, OpenError>> + Send + 'a>>;

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
                .await
                .map_err(|e: String| OpenError::Failed(e))?;
            let mut stream = self
                .client
                .open_stream(StreamConfig::new(&self.engine, 16_000))
                .await
                .map_err(|e| match e {
                    fotw_speechd_client::ClientError::Remote {
                        code: fotw_speechd_client::message::ErrorCode::Busy,
                        message,
                    } => OpenError::Busy(message),
                    other => OpenError::Failed(other.to_string()),
                })?;
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

/// The open caption stream: its session, the leg sample its first audio was
/// at (once it has had any), and its number within the recording (for
/// segment ids).
struct Open {
    session: CaptionSession,
    base: Option<u64>,
    generation: u64,
}

/// After a failed or refused open, how long to wait before the next try.
const RETRY_AFTER: Duration = Duration::from_millis(500);

/// How long to let an ending stream deliver its last captions and hand its
/// engine instance back before the next stream opens.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// An open that takes longer than this (the first one loads and warms the
/// model) has let stale audio queue up behind it.
const SLOW_OPEN: Duration = Duration::from_millis(50);

async fn run(leg: CaptionLeg, mut audio: mpsc::Receiver<(u64, Vec<i16>)>) {
    let model = leg.engine.model();
    let mut open: Option<Open> = None;
    let mut next_sample = 0u64;
    let mut generation = 0u64;
    let mut errors = 0u32;
    let mut retry_at: Option<tokio::time::Instant> = None;
    (leg.state)(leg.source, CaptionState::Live);
    loop {
        tokio::select! {
            chunk = audio.recv() => {
                // The leg ended: let the open stream commit what it has.
                let Some((start, pcm)) = chunk else { break };
                let len = pcm.len() as u64;
                // A gap (dropped chunks): finish this stream, and only then
                // start one on the leg's new position, so the old stream's
                // engine instance is back before the new one asks for it.
                if open.as_ref().is_some_and(|o| o.base.is_some())
                    && start != next_sample
                    && let Some(old) = open.take()
                {
                    finish(&leg, &model, old).await;
                }
                if open.is_none() {
                    if retry_at.is_some_and(|at| tokio::time::Instant::now() < at) {
                        continue;
                    }
                    generation += 1;
                    let opening = tokio::time::Instant::now();
                    match leg.engine.open().await {
                        Ok(session) => {
                            retry_at = None;
                            open = Some(Open { session, base: None, generation });
                            if opening.elapsed() > SLOW_OPEN {
                                // Audio queued (and overflowed) while it
                                // opened: drop it, and start on fresh audio
                                // rather than on a gap that restarts at once.
                                while audio.try_recv().is_ok() {}
                                continue;
                            }
                        }
                        Err(OpenError::Busy(_)) => {
                            retry_at = Some(tokio::time::Instant::now() + RETRY_AFTER);
                            continue;
                        }
                        Err(OpenError::Failed(_)) => {
                            errors += 1;
                            if errors >= MAX_CONSECUTIVE_ERRORS {
                                break;
                            }
                            retry_at = Some(tokio::time::Instant::now() + RETRY_AFTER);
                            continue;
                        }
                    }
                }
                let Some(current) = open.as_mut() else { continue };
                if current.base.is_none() {
                    current.base = Some(start);
                }
                next_sample = start + len;
                if current.session.audio.try_send(pcm).is_err() {
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
                Some(CaptionEvent::Partial(text) | CaptionEvent::Final(text)) => {
                    errors = 0;
                    if let Some(current) = &open
                        && leg.events.send(StreamEvent::Partial(caption_segment(&leg, &model, current, text))).is_err()
                    {
                        return;
                    }
                }
                Some(CaptionEvent::Failed(_)) => {
                    open = None;
                    errors += 1;
                    if errors >= MAX_CONSECUTIVE_ERRORS {
                        break;
                    }
                    retry_at = Some(tokio::time::Instant::now() + RETRY_AFTER);
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
    if let Some(current) = open.take() {
        finish(&leg, &model, current).await;
    }
}

/// End a stream: no more audio, then its remaining captions until it closes
/// (by which point its engine instance is free again), bounded.
async fn finish(leg: &CaptionLeg, model: &str, mut current: Open) {
    let (closed, _) = mpsc::channel(1);
    current.session.audio = closed;
    let deadline = tokio::time::Instant::now() + CLOSE_WAIT;
    while let Ok(Some(event)) =
        tokio::time::timeout_at(deadline, current.session.events.recv()).await
    {
        if let CaptionEvent::Partial(text) | CaptionEvent::Final(text) = event {
            let segment = caption_segment(leg, model, &current, text);
            if leg.events.send(StreamEvent::Partial(segment)).is_err() {
                return;
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
    let base_ms = leg.anchor_ms + open.base.unwrap_or(0) / SAMPLES_PER_MS;
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

    /// A caption engine scripted per opened stream: each stream echoes a
    /// partial per audio chunk ("<n> samples"), or fails on open or on its
    /// first chunk.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Script {
        Echo,
        FailOnOpen,
        FailOnAudio,
        /// Refused as busy, like a pool whose instance is not back yet.
        Busy,
        /// Echo, after a slow first open (load and warmup).
        SlowOpen,
    }

    struct Scripted {
        scripts: Mutex<Vec<Script>>,
        opened: AtomicU64,
        /// Streams open right now, and the most ever open at once.
        live: Arc<AtomicU64>,
        most_live: Arc<AtomicU64>,
    }

    impl Scripted {
        fn new(scripts: &[Script]) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.iter().rev().copied().collect()),
                opened: AtomicU64::new(0),
                live: Arc::default(),
                most_live: Arc::default(),
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
                match script {
                    Script::FailOnOpen => {
                        return Err(OpenError::Failed("scripted open failure".into()));
                    }
                    Script::Busy => return Err(OpenError::Busy("scripted busy".into())),
                    Script::SlowOpen => tokio::time::sleep(Duration::from_millis(300)).await,
                    _ => {}
                }
                let live = self.live.clone();
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                self.most_live.fetch_max(now, Ordering::SeqCst);
                let (audio, mut pcm) = mpsc::channel::<Vec<i16>>(8);
                let (events, receiver) = mpsc::channel(64);
                tokio::spawn(async move {
                    let _live = Live(live);
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

    /// Counts a scripted stream as live until its task ends.
    struct Live(Arc<AtomicU64>);
    impl Drop for Live {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Offer `count` 10 ms chunks, `every` apart.
    async fn feed(feed: &CaptionFeed, count: usize, every: Duration) {
        for _ in 0..count {
            feed.offer(&[1; 160]);
            tokio::time::sleep(every).await;
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
        // Opens are retried 500 ms apart.
        self::feed(&feed, 40, Duration::from_millis(50)).await;
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
        self::feed(&feed, 30, Duration::from_millis(50)).await;
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
        // The old stream is finished first and may deliver its last caption.
        let mut after = next(&mut rx).await;
        while after.id == first.id {
            after = next(&mut rx).await;
        }
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

    #[tokio::test]
    async fn busy_engines_are_waited_out_and_never_pause_captions() {
        // #245 review: right after a restart the engine instance may not be
        // back yet; that is not a failure.
        let engine = Scripted::new(&[
            Script::Busy,
            Script::Busy,
            Script::Busy,
            Script::Busy,
            Script::Echo,
        ]);
        let (leg, mut rx, states) = leg(engine.clone());
        let feed = start(leg);
        self::feed(&feed, 60, Duration::from_millis(50)).await;
        assert!(next(&mut rx).await.text.ends_with("samples"));
        assert_eq!(*states.lock().unwrap(), vec![CaptionState::Live]);
        assert_eq!(
            engine.opened.load(Ordering::SeqCst),
            5,
            "retried 500 ms apart, not every chunk"
        );
    }

    #[tokio::test]
    async fn a_gap_finishes_the_old_stream_before_the_next_one_opens() {
        let engine = Scripted::new(&[Script::Echo, Script::Echo]);
        let (leg, mut rx, _) = leg(engine.clone());
        let feed = start(leg);
        feed.offer(&[1; 160]);
        let _ = next(&mut rx).await;
        feed.position.fetch_add(16_000, Ordering::Relaxed);
        feed.offer(&[1; 160]);
        // The old stream's last caption, then the new stream's first.
        let _ = next(&mut rx).await;
        let _ = next(&mut rx).await;
        assert_eq!(engine.opened.load(Ordering::SeqCst), 2);
        assert_eq!(
            engine.most_live.load(Ordering::SeqCst),
            1,
            "never two streams at once"
        );
    }

    #[tokio::test]
    async fn a_slow_first_open_starts_on_fresh_audio_instead_of_restarting() {
        // #245 review: the first open loads and warms the model while the
        // 64-chunk queue overflows; that must not restart the stream at once.
        let engine = Scripted::new(&[Script::SlowOpen]);
        let (leg, mut rx, states) = leg(engine.clone());
        let feed = start(leg);
        for _ in 0..200 {
            feed.offer(&[1; 160]);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let caption = next(&mut rx).await;
        assert!(caption.text.ends_with("samples"));
        self::feed(&feed, 10, Duration::from_millis(10)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            engine.opened.load(Ordering::SeqCst),
            1,
            "one stream, no restart"
        );
        assert_eq!(*states.lock().unwrap(), vec![CaptionState::Live]);
    }
}
