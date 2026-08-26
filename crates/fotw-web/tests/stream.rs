//! The live transcript stream, over a real WebSocket — ING-06, ING-07, §5.5.
//!
//! # Why these tests are the ones that matter
//!
//! §10.1 says it outright: *same-origin integration tests will not catch this.*
//! A test that opens a WebSocket from the right origin with a valid ticket
//! passes against a handler that checks nothing at all. So every test here
//! that asserts a refusal sets the header a browser would never let a page set
//! — which is exactly what a hostile page **can** set, because the WebSocket
//! handshake is exempt from the same-origin policy and `new WebSocket()` needs
//! no permission, no preflight and no user gesture.

mod common;

use std::time::Duration;

use fotw_web::{Delta, MeetingReadyReason};
use futures_util::StreamExt as _;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open a handshake with whatever `Host` and `Origin` the test wants.
///
/// Built from a real client request and then overwritten, so everything else
/// on the wire — `Sec-WebSocket-Key`, `Connection`, `Upgrade`, the version —
/// is a valid handshake. A refusal here is therefore about the header under
/// test and not about a malformed request.
async fn connect(
    h: &common::Harness,
    ticket: &str,
    host: Option<&str>,
    origin: Option<&str>,
) -> Result<Client, WsError> {
    let url = format!("ws://{}/api/stream?ticket={ticket}", h.authority);
    let mut request = url.into_client_request().expect("a valid ws url");
    if let Some(host) = host {
        request
            .headers_mut()
            .insert("host", HeaderValue::from_str(host).unwrap());
    }
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", HeaderValue::from_str(origin).unwrap());
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
}

fn refusal_status(err: WsError) -> u16 {
    match err {
        WsError::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other}"),
    }
}

async fn ticket(h: &common::Harness) -> String {
    let res = h.post("/api/ws-ticket", &h.authorised(), None).await;
    assert_eq!(res.status, 200, "minting a ticket must work");
    let value: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    value["ticket"].as_str().unwrap().to_owned()
}

/// Wait until the server side of the upgrade has subscribed to the hub.
async fn await_subscriber(h: &common::Harness) {
    for _ in 0..200 {
        if h.state.hub().subscriber_count() > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never attached a subscriber to the hub");
}

// ------------------------------------------------------------------- ING-06

/// The attack. A page on `evil.test` runs
/// `new WebSocket("ws://127.0.0.1:51234/api/stream?ticket=…")` and, if nothing
/// checks, starts receiving the transcript of the meeting the user is in right
/// now. CORS has no opinion about this and `axum` performs no check —
/// `grep -ic origin axum-0.8.9/src/extract/ws.rs` is `0`.
#[tokio::test]
async fn a_hostile_origin_cannot_open_the_transcript_stream() {
    let h = common::start().await;
    for origin in [
        "http://evil.test",
        "https://evil.test",
        "http://127.0.0.1:1",
        "null",
    ] {
        let t = ticket(&h).await;
        let err = connect(&h, &t, None, Some(origin))
            .await
            .err()
            .unwrap_or_else(|| panic!("{origin} must not complete the handshake"));
        assert_eq!(refusal_status(err), 404, "{origin} must get a bare 404");
        assert!(
            !h.state.tickets().is_empty(),
            "the origin check must run before the ticket is spent, so a \
             hostile handshake cannot burn the real page's ticket"
        );
        // Clean up so the next iteration starts from a known table.
        assert!(h.state.tickets().redeem(&t));
    }
}

/// The same page after DNS rebinding: same-origin as far as the browser is
/// concerned, so `Origin` now *matches*. The raw `Host` is what is left.
#[tokio::test]
async fn a_rebound_host_cannot_open_the_transcript_stream() {
    let h = common::start().await;
    let t = ticket(&h).await;
    let err = connect(&h, &t, Some("evil.test"), Some("http://evil.test"))
        .await
        .expect_err("a rebound handshake must be refused");
    assert_eq!(refusal_status(err), 404);
}

// ------------------------------------------------------------------- ING-07

#[tokio::test]
async fn no_ticket_no_stream() {
    let h = common::start().await;
    for t in ["", "not-a-ticket", &"a".repeat(64), &h.token] {
        let err = connect(&h, t, None, Some(&h.origin))
            .await
            .err()
            .unwrap_or_else(|| panic!("ticket {t:?} must be refused"));
        assert_eq!(refusal_status(err), 404);
    }
}

#[tokio::test]
async fn a_ticket_opens_one_stream_and_not_two() {
    let h = common::start().await;
    let t = ticket(&h).await;

    let first = connect(&h, &t, None, Some(&h.origin))
        .await
        .expect("the first connection must succeed");
    await_subscriber(&h).await;

    let err = connect(&h, &t, None, Some(&h.origin))
        .await
        .expect_err("a replayed ticket must not open a second stream");
    assert_eq!(refusal_status(err), 404);
    drop(first);
}

// -------------------------------------------------------------------- §5.5

/// The batching rule, over the wire: 300 deltas inside one tick arrive as
/// **one** WebSocket frame. One message per word would be 300.
#[tokio::test]
async fn deltas_arrive_batched_not_one_message_per_word() {
    let h = common::start().await;
    let t = ticket(&h).await;
    let mut socket = connect(&h, &t, None, Some(&h.origin))
        .await
        .expect("the real page must be able to connect");
    await_subscriber(&h).await;

    for i in 0..300 {
        h.state.hub().publish(Delta {
            meeting_id: common::MEETING_ID.to_owned(),
            idx: i,
            start_ms: i * 100,
            end_ms: i * 100 + 100,
            channel: "mic".to_owned(),
            text: format!("word{i}"),
            is_final: false,
        });
    }
    assert_eq!(h.state.hub().flush(), 300);

    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("a frame must arrive")
        .expect("the socket must stay open")
        .expect("a well-formed frame");
    let Message::Text(json) = message else {
        panic!("expected a text frame, got {message:?}");
    };
    let frame: fotw_web::Frame = serde_json::from_str(&json).unwrap();
    assert_eq!(frame.kind, "deltas");
    assert_eq!(
        frame.deltas.len(),
        300,
        "§5.5: one frame per tick, not one per word"
    );
    assert_eq!(frame.deltas[0].text, "word0");
    assert_eq!(frame.deltas[299].text, "word299");

    // And nothing else follows, because nothing else was published.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), socket.next())
            .await
            .is_err(),
        "a second frame would mean the batch was split"
    );
}

// --------------------------------------------------------------------- #78

/// The frame that stops the library going stale, over a real socket.
///
/// The first non-delta frame this suite has ever asserted: `resync` has
/// travelled the same `broadcast::Sender<Arc<str>>` since the UI shipped with
/// no test covering it, which is how #67 came to claim `resync` already told
/// an open tab to refetch. It does not — it fires on lag and nothing else.
/// This one fires when a meeting lands.
#[tokio::test]
async fn a_meeting_ready_frame_reaches_an_open_socket() {
    let h = common::start().await;
    let t = ticket(&h).await;
    let mut socket = connect(&h, &t, None, Some(&h.origin))
        .await
        .expect("the real page must be able to connect");
    await_subscriber(&h).await;

    h.state
        .hub()
        .announce_meeting_ready(common::MEETING_ID, MeetingReadyReason::Persisted);

    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the announcement must arrive without waiting for a tick")
        .expect("the socket must stay open")
        .expect("a well-formed frame");
    let Message::Text(json) = message else {
        panic!("expected a text frame, got {message:?}");
    };
    let frame: fotw_web::MeetingReady = serde_json::from_str(&json).unwrap();
    assert_eq!(frame.kind, "meeting_ready");
    assert_eq!(frame.meeting_id, common::MEETING_ID);
    assert_eq!(frame.reason, MeetingReadyReason::Persisted);

    // The socket is still live afterwards: a library event must not disturb
    // the transcript stream it shares a channel with.
    h.state.hub().publish(Delta {
        meeting_id: common::MEETING_ID.to_owned(),
        idx: 0,
        start_ms: 0,
        end_ms: 100,
        channel: "system".to_owned(),
        text: "still here".to_owned(),
        is_final: true,
    });
    assert_eq!(h.state.hub().flush(), 1);
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("deltas must still flow")
        .expect("the socket must stay open")
        .expect("a well-formed frame");
    let Message::Text(json) = message else {
        panic!("expected a text frame, got {message:?}");
    };
    let frame: fotw_web::Frame = serde_json::from_str(&json).unwrap();
    assert_eq!(frame.kind, "deltas");
    assert_eq!(frame.deltas[0].text, "still here");
}

#[tokio::test]
async fn an_idle_meeting_sends_nothing() {
    let h = common::start().await;
    let t = ticket(&h).await;
    let mut socket = connect(&h, &t, None, Some(&h.origin)).await.unwrap();
    await_subscriber(&h).await;

    for _ in 0..5 {
        assert_eq!(h.state.hub().flush(), 0);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(300), socket.next())
            .await
            .is_err(),
        "an empty tick must not wake the browser"
    );
}
