//! The live transcript stream: ING-06, ING-07, and §5.5's 10 Hz batching.
//!
//! # ING-06 — `axum` does not check `Origin`, and nothing else will either
//!
//! `axum::extract::WebSocketUpgrade` performs **zero** origin validation.
//! `grep -ic origin` over `axum-0.8.9/src/extract/ws.rs` returns `0`; the
//! extractor checks the method, `Connection`, `Upgrade`, `Sec-WebSocket-Key`
//! and `Sec-WebSocket-Version`, and then hands you an upgrade. Nor does the
//! browser help: the WebSocket handshake is **exempt from the same-origin
//! policy**, so a page on `evil.test` may open a socket to any host it likes
//! and read every byte that comes back. There is no preflight, and CORS has no
//! say.
//!
//! What comes back here is the live transcript of a meeting that is happening
//! right now. So the check has to be ours, it has to be explicit, and it has
//! to run **before `on_upgrade`** — after the upgrade the response is a 101
//! and there is no longer a status code to refuse with.
//!
//! A same-origin integration test cannot catch the absence of this check,
//! which is why [`authorize_upgrade`] is a separate function with its own
//! tests: one connects with `Origin: http://evil.test`, and deleting the
//! origin arm of that function turns it red.
//!
//! # §5.5 — batched at 10 Hz, never one message per word
//!
//! Deepgram emits interim results several times a second per channel, and a
//! two-hour meeting is roughly 20,000 words. One WebSocket frame per word is
//! ~20,000 frames, each with its own JSON envelope, its own wake-up and its
//! own React-ish re-render, against a budget of <3 % average CPU. So
//! [`DeltaHub::publish`] only buffers — it never sends, and never awaits — and
//! a 10 Hz tick drains the buffer into exactly one frame. The frame is
//! serialised once and shared by every subscriber rather than once per socket.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use futures_util::SinkExt;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::ingress::{Deny, not_found};
use crate::query::query_param;
use crate::state::AppState;

/// §5.5: 10 Hz. One frame per 100 ms, however many words arrived in it.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// How many frames a slow subscriber may fall behind before it is told to
/// resynchronise.
///
/// Small on purpose. A browser that has stopped reading is a browser whose tab
/// is in the background; holding minutes of transcript for it would grow the
/// daemon's RSS against §5.5's 50 MB soak budget, and the data it missed is
/// already durable in the store — it can re-fetch.
const BACKLOG: usize = 64;

/// One increment of transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    /// The meeting this belongs to.
    pub meeting_id: String,
    /// Segment index within the transcript.
    pub idx: i64,
    /// Offset from meeting start.
    pub start_ms: i64,
    /// End offset.
    pub end_ms: i64,
    /// `mic` or `system` — §7.5's two-stream capture is what makes "me vs
    /// them" free, and losing it here would turn it back into a diarisation
    /// problem in the UI.
    pub channel: String,
    /// The words. Attacker-influenced (see [`crate::source::Segment::text`]).
    pub text: String,
    /// Whether the provider considers this text settled.
    pub is_final: bool,
}

/// What a subscriber receives: one 100 ms batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    /// Always `deltas` on this struct. The switch it was put here for is now
    /// real: `resync` and [`MeetingReady`]'s `meeting_ready` share the
    /// channel, and the client dispatches on this field rather than guessing
    /// from shape.
    pub kind: String,
    /// Every delta published since the previous tick, in publication order.
    pub deltas: Vec<Delta>,
}

/// Why a meeting just became worth re-fetching (#78).
///
/// The client treats both identically — refresh the list, redraw the pane if
/// it is the one open. They are distinguished for tests and for anyone
/// watching the wire, where "the row exists" and "the row finally has a
/// title" are minutes apart on a long meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeetingReadyReason {
    /// The row is in the library and queryable. Sent before promotion: the
    /// Opus encode can take minutes and the meeting is already listable.
    Persisted,
    /// Enrichment finished, so the title and summary are final.
    Enriched,
}

/// The library-changed frame: a meeting is queryable, go and fetch it.
///
/// Nothing tells an open dashboard that a recording finished, so the list
/// stayed stale until the tab was reloaded (#78). This is that event. It
/// carries no `deltas` field on purpose — a client from before this frame
/// existed falls through to `appendDeltas(frame.deltas || [])`, which no-ops
/// on an empty array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingReady {
    /// Always `meeting_ready`.
    pub kind: String,
    /// The meeting to fetch — the library id, not the session id the live
    /// deltas carry. The client matches it against the open detail pane.
    pub meeting_id: String,
    /// Which of the two moments this is.
    pub reason: MeetingReadyReason,
}

/// The fan-out point between the pipeline and every open WebSocket.
#[derive(Debug)]
pub struct DeltaHub {
    pending: Mutex<Vec<Delta>>,
    tx: broadcast::Sender<Arc<str>>,
}

impl Default for DeltaHub {
    fn default() -> Self {
        Self::new()
    }
}

impl DeltaHub {
    /// A hub with no subscribers.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BACKLOG);
        Self {
            pending: Mutex::new(Vec::new()),
            tx,
        }
    }

    /// Buffer a delta for the next tick.
    ///
    /// Synchronous and non-blocking by design: the caller is the STT pump,
    /// which must not be made to wait on a browser. This function does not
    /// send anything — [`DeltaHub::flush`] does, at most ten times a second.
    pub fn publish(&self, delta: Delta) {
        self.lock().push(delta);
    }

    /// Drain the buffer into one frame and broadcast it.
    ///
    /// Returns the number of deltas that went out, so a caller can assert the
    /// batching rather than infer it. Sends nothing at all when the buffer is
    /// empty: a silent meeting must not cost ten wake-ups a second on every
    /// open socket.
    pub fn flush(&self) -> usize {
        let deltas: Vec<Delta> = std::mem::take(&mut *self.lock());
        if deltas.is_empty() {
            return 0;
        }
        let n = deltas.len();
        let frame = Frame {
            kind: "deltas".to_owned(),
            deltas,
        };
        // Serialised once for all subscribers. The alternative — handing each
        // socket the `Vec<Delta>` and letting it serialise — is O(sockets)
        // JSON encodes of the same bytes, ten times a second.
        let Ok(json) = serde_json::to_string(&frame) else {
            return 0;
        };
        // `send` fails only when there are no subscribers, which is the normal
        // state of a meeting nobody has opened a tab for.
        let _ = self.tx.send(Arc::from(json.as_str()));
        n
    }

    /// Tell every open tab that a meeting is worth fetching (#78).
    ///
    /// Not a [`DeltaHub::publish`]: that buffers a `Delta`, and
    /// [`DeltaHub::flush`] hard-codes `kind: "deltas"` and drops an empty
    /// batch, so a non-delta frame cannot ride the batching path at all. It
    /// goes straight down the channel underneath, which is kind-agnostic —
    /// exactly how `resync` already travels.
    ///
    /// Flushes first, so anything already buffered keeps its place in
    /// publication order: the finisher announces from a blocking thread that
    /// can land between two ticks, and a browser told the meeting is over
    /// before it was told the last sentence of it would draw them in the wrong
    /// order.
    ///
    /// Sends immediately rather than waiting for a tick — the point of the
    /// frame is that the tab stops being stale *now* — and is synchronous and
    /// non-blocking, because the caller is the `spawn_blocking` thread the
    /// session finisher runs on and must not be made to wait on a browser.
    pub fn announce_meeting_ready(&self, meeting_id: &str, reason: MeetingReadyReason) {
        self.flush();
        let frame = MeetingReady {
            kind: "meeting_ready".to_owned(),
            meeting_id: meeting_id.to_owned(),
            reason,
        };
        let Ok(json) = serde_json::to_string(&frame) else {
            return;
        };
        // As in `flush`: no subscribers is the normal state of a daemon
        // nobody has a tab open on, and it must never fail a persist.
        let _ = self.tx.send(Arc::from(json.as_str()));
    }

    /// A receiver for every frame broadcast from now on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }

    /// How many sockets are listening.
    ///
    /// The daemon can skip work nobody is watching, and `tests/stream.rs` uses
    /// it to wait for the server side of a handshake to attach before
    /// publishing — otherwise the test races the upgrade and fails one time in
    /// a hundred on a loaded runner, which is how a real assertion gets
    /// deleted for being "flaky".
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Run the 10 Hz tick until the hub is dropped.
    ///
    /// Held by the daemon; tests drive [`DeltaHub::flush`] directly so that
    /// what they assert is the batching rule and not the wall clock.
    pub fn spawn_flusher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let hub = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match hub.upgrade() {
                    Some(hub) => {
                        hub.flush();
                    }
                    None => break,
                }
            }
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Delta>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Everything that must be true before a socket is upgraded — ING-06 and
/// ING-07 in one place, so that "before `on_upgrade`" is a property of the
/// code rather than of the reviewer's attention.
///
/// The `Origin` check duplicates the one in [`crate::ingress::guard`]. That is
/// deliberate: this is the endpoint where a missing origin check is not "an
/// API is exposed" but "a stranger is listening to the meeting", and it should
/// survive someone mounting this route on a different router.
///
/// # Errors
///
/// [`Deny::OriginNotAllowed`] or [`Deny::TicketInvalid`]. The caller maps both
/// to the same bare 404 (ING-09).
pub fn authorize_upgrade(
    state: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<(), Deny> {
    // ING-06. A browser always sends `Origin` on a WebSocket handshake and a
    // page cannot forge it, so a hostile page cannot reach the `Ok` below.
    state.policy().check_origin(headers)?;

    // ING-07. In the query string because `new WebSocket(url, protocols)` has
    // nowhere else to put it — see `crate::tokens`.
    let ticket = query
        .and_then(|q| query_param(q, "ticket"))
        .ok_or(Deny::TicketInvalid)?;
    if state.tickets().redeem(&ticket) {
        Ok(())
    } else {
        Err(Deny::TicketInvalid)
    }
}

/// `GET /api/stream`.
pub async fn stream(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    // Not a bare `WebSocketUpgrade`: its rejection renders as a 400 whose body
    // names the missing handshake header, and that is a fingerprint (ING-09).
    // Extraction itself is inert — it captures `OnUpgrade` and writes nothing
    // — so the checks below still run before any upgrade happens.
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if authorize_upgrade(&state, &headers, uri.query()).is_err() {
        return not_found();
    }
    let Ok(upgrade) = upgrade else {
        return not_found();
    };
    upgrade.on_upgrade(move |socket| pump(socket, state))
}

/// Copy batched frames to one socket until either end goes away.
async fn pump(socket: WebSocket, state: AppState) {
    let mut rx = state.hub().subscribe();
    let (mut sink, mut incoming) = socket.split();
    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Ok(json) => {
                    if sink.send(Message::Text(json.as_ref().into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // The client fell behind by more than BACKLOG frames.
                    // Everything it missed is already in the store, so tell it
                    // to re-fetch rather than trying to replay from memory.
                    if sink
                        .send(Message::Text(r#"{"kind":"resync"}"#.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // The read half exists to notice `Close` and TCP resets. Anything
            // the client sends is ignored: this socket is one-directional by
            // design, and an endpoint that accepts commands over a channel
            // authenticated by a ten-second ticket is a bigger promise than
            // ING-07 makes.
            incoming = incoming.next() => match incoming {
                Some(Ok(_)) => {}
                _ => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress::IngressPolicy;
    use crate::source::MemorySource;

    fn state() -> AppState {
        AppState::new(
            IngressPolicy::for_loopback_port(51234),
            Arc::new(MemorySource::new()),
        )
    }

    fn delta(text: &str) -> Delta {
        Delta {
            meeting_id: "m1".into(),
            idx: 0,
            start_ms: 0,
            end_ms: 100,
            channel: "mic".into(),
            text: text.into(),
            is_final: false,
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    // ------------------------------------------------------------- §5.5

    /// The batching rule, stated as the thing it forbids: 250 words in one
    /// tick must produce **one** frame, not 250.
    #[tokio::test]
    async fn a_tick_emits_one_frame_however_many_words_arrived() {
        let hub = Arc::new(DeltaHub::new());
        let mut rx = hub.subscribe();

        for i in 0..250 {
            hub.publish(delta(&format!("word{i}")));
        }
        assert_eq!(hub.flush(), 250);

        let json = rx.try_recv().expect("exactly one frame");
        let frame: Frame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame.kind, "deltas");
        assert_eq!(frame.deltas.len(), 250, "all 250 must be in the one frame");
        assert_eq!(frame.deltas[0].text, "word0", "order must be preserved");
        assert_eq!(frame.deltas[249].text, "word249");
        assert!(
            rx.try_recv().is_err(),
            "a second frame means this is one-message-per-word with extra steps"
        );
    }

    #[tokio::test]
    async fn silence_costs_nothing() {
        let hub = Arc::new(DeltaHub::new());
        let mut rx = hub.subscribe();
        assert_eq!(hub.flush(), 0);
        assert_eq!(hub.flush(), 0);
        assert!(
            rx.try_recv().is_err(),
            "an empty tick must not wake every open socket"
        );
    }

    #[tokio::test]
    async fn publishing_does_not_send_before_the_tick() {
        let hub = Arc::new(DeltaHub::new());
        let mut rx = hub.subscribe();
        hub.publish(delta("hello"));
        assert!(
            rx.try_recv().is_err(),
            "publish must buffer; only flush sends"
        );
        hub.flush();
        assert!(rx.try_recv().is_ok());
    }

    /// The real 10 Hz tick, on a paused clock so the assertion is about the
    /// interval and not about how busy the machine is.
    #[tokio::test(start_paused = true)]
    async fn the_flusher_ticks_ten_times_a_second() {
        let hub = Arc::new(DeltaHub::new());
        let mut rx = hub.subscribe();
        let _task = hub.spawn_flusher();

        // One second of a fast talker: 200 words, arriving 20 ms apart.
        for i in 0..50 {
            for w in 0..4 {
                hub.publish(delta(&format!("w{i}_{w}")));
            }
            tokio::time::advance(Duration::from_millis(20)).await;
        }
        tokio::time::advance(FLUSH_INTERVAL).await;
        tokio::task::yield_now().await;

        let mut frames = 0;
        let mut words = 0;
        while let Ok(json) = rx.try_recv() {
            let frame: Frame = serde_json::from_str(&json).unwrap();
            words += frame.deltas.len();
            frames += 1;
        }
        assert_eq!(words, 200, "no word may be dropped by the batching");
        assert!(
            frames <= 12,
            "one second at 10 Hz is ~10 frames, got {frames} — that is \
             one-message-per-word territory"
        );
        assert!(
            frames >= 2,
            "got {frames} frames; the flusher is not running"
        );
    }

    // --------------------------------------------------------------- #78

    /// The frame the library refresh rides on, pinned byte for byte.
    ///
    /// Exact JSON rather than a round-trip through [`MeetingReady`], because
    /// the consumer is `app.js` and it switches on the literal strings
    /// `meeting_ready` and reads `frame.meeting_id`. A rename on this side
    /// that still deserialises here would leave every open tab stale again,
    /// which is the bug.
    #[tokio::test]
    async fn an_announcement_reaches_subscribers_without_waiting_for_a_tick() {
        let hub = Arc::new(DeltaHub::new());
        let mut rx = hub.subscribe();

        hub.announce_meeting_ready("m-42", MeetingReadyReason::Persisted);

        let json = rx.try_recv().expect("no tick may be needed");
        assert_eq!(
            json.as_ref(),
            r#"{"kind":"meeting_ready","meeting_id":"m-42","reason":"persisted"}"#
        );

        hub.announce_meeting_ready("m-42", MeetingReadyReason::Enriched);
        let json = rx.try_recv().expect("the second announcement too");
        assert_eq!(
            json.as_ref(),
            r#"{"kind":"meeting_ready","meeting_id":"m-42","reason":"enriched"}"#
        );
    }

    /// Publication order, which is the whole reason this flushes first.
    ///
    /// The finisher announces from a blocking thread that can land between
    /// two 10 Hz ticks, so the last words of the meeting may still be sitting
    /// in the buffer. Sending "ready" ahead of them would put the meeting's
    /// closing sentence on screen *after* the library said it was done.
    #[tokio::test]
    async fn an_announcement_flushes_the_buffered_deltas_first() {
        let hub = Arc::new(DeltaHub::new());
        let mut rx = hub.subscribe();

        hub.publish(delta("the last thing anyone said"));
        hub.announce_meeting_ready("m-42", MeetingReadyReason::Persisted);

        let first: Frame = serde_json::from_str(&rx.try_recv().expect("the deltas")).unwrap();
        assert_eq!(first.kind, "deltas");
        assert_eq!(first.deltas[0].text, "the last thing anyone said");

        let second: MeetingReady =
            serde_json::from_str(&rx.try_recv().expect("then the announcement")).unwrap();
        assert_eq!(second.meeting_id, "m-42");
        assert_eq!(second.reason, MeetingReadyReason::Persisted);
    }

    /// The normal state of a daemon nobody has opened a tab for. The finisher
    /// must not learn about it, and must certainly not fail a persist over it.
    #[tokio::test]
    async fn announcing_to_nobody_is_not_an_error() {
        let hub = Arc::new(DeltaHub::new());
        hub.announce_meeting_ready("m-42", MeetingReadyReason::Persisted);
        assert_eq!(hub.subscriber_count(), 0);
    }

    /// An announcement is not a tick: it must not manufacture an empty
    /// `deltas` frame on the way past, or a silent meeting starts costing the
    /// wake-ups `silence_costs_nothing` exists to forbid.
    #[tokio::test]
    async fn an_announcement_over_an_empty_buffer_sends_exactly_one_frame() {
        let hub = Arc::new(DeltaHub::new());
        let mut rx = hub.subscribe();
        hub.announce_meeting_ready("m-42", MeetingReadyReason::Enriched);
        assert!(rx.try_recv().is_ok(), "the announcement");
        assert!(
            rx.try_recv().is_err(),
            "an empty flush must not have sent a `deltas` frame as well"
        );
    }

    // ------------------------------------------------------------- ING-06

    /// The attack, as a unit test. A page on `evil.test` opens a WebSocket —
    /// which the same-origin policy does not stop and `axum` does not check —
    /// and reads the transcript of a meeting in progress.
    #[test]
    fn a_hostile_origin_cannot_upgrade_even_with_a_valid_ticket() {
        let state = state();
        let ticket = state.tickets().mint();
        let query = format!("ticket={ticket}");
        for origin in [
            "http://evil.test",
            "https://evil.test",
            "http://127.0.0.1:51235",
            "http://localhost:51234",
            "null",
        ] {
            assert_eq!(
                authorize_upgrade(&state, &headers(&[("origin", origin)]), Some(&query)),
                Err(Deny::OriginNotAllowed),
                "{origin} must not be able to open the stream"
            );
        }
    }

    #[test]
    fn our_own_origin_with_a_fresh_ticket_may_upgrade() {
        let state = state();
        let ticket = state.tickets().mint();
        assert_eq!(
            authorize_upgrade(
                &state,
                &headers(&[("origin", "http://127.0.0.1:51234")]),
                Some(&format!("ticket={ticket}")),
            ),
            Ok(())
        );
    }

    // ------------------------------------------------------------- ING-07

    #[test]
    fn no_ticket_no_stream() {
        let state = state();
        let h = headers(&[("origin", "http://127.0.0.1:51234")]);
        assert_eq!(
            authorize_upgrade(&state, &h, None),
            Err(Deny::TicketInvalid)
        );
        assert_eq!(
            authorize_upgrade(&state, &h, Some("")),
            Err(Deny::TicketInvalid)
        );
        assert_eq!(
            authorize_upgrade(&state, &h, Some("ticket=")),
            Err(Deny::TicketInvalid)
        );
        assert_eq!(
            authorize_upgrade(&state, &h, Some("ticket=deadbeef")),
            Err(Deny::TicketInvalid)
        );
        // The bearer token is not a ticket: minting is a separate,
        // authenticated step, so a leaked stream URL is not a leaked session.
        let bearer = state.policy().secret().expose_hex();
        assert_eq!(
            authorize_upgrade(&state, &h, Some(&format!("ticket={bearer}"))),
            Err(Deny::TicketInvalid)
        );
    }

    #[test]
    fn a_ticket_opens_exactly_one_stream() {
        let state = state();
        let ticket = state.tickets().mint();
        let h = headers(&[("origin", "http://127.0.0.1:51234")]);
        let q = format!("ticket={ticket}");
        assert_eq!(authorize_upgrade(&state, &h, Some(&q)), Ok(()));
        assert_eq!(
            authorize_upgrade(&state, &h, Some(&q)),
            Err(Deny::TicketInvalid),
            "a replayed ticket must not open a second stream"
        );
    }

    /// The ordering that ING-06 is about: a refused origin must not spend the
    /// ticket, because the check that runs first is the one that decides.
    #[test]
    fn the_origin_check_runs_before_the_ticket_is_spent() {
        let state = state();
        let ticket = state.tickets().mint();
        let q = format!("ticket={ticket}");
        let _ = authorize_upgrade(
            &state,
            &headers(&[("origin", "http://evil.test")]),
            Some(&q),
        );
        assert_eq!(
            state.tickets().len(),
            1,
            "the hostile handshake must not have consumed the ticket"
        );
    }
}
