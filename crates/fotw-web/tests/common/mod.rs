//! A running daemon and a deliberately stupid HTTP client.
//!
//! The client speaks raw HTTP/1.1 over a `TcpStream` rather than going through
//! `reqwest` or `hyper`'s client, and that is the point: **every request in
//! this suite is one a well-behaved client would refuse to make.** `Host:
//! evil.test` on a connection to `127.0.0.1`, two `Host` headers, an `Origin`
//! that does not match the connection — a real client normalises all of that
//! away, which would leave the tests asserting that the server handles
//! requests nobody can send. A browser under DNS rebinding sends exactly these
//! bytes.
//!
//! It also means the tests can compare **response bytes**, which ING-09 asks
//! for and which no client API exposes.

#![allow(dead_code)]

use std::net::SocketAddr;

use fotw_web::{AppState, MeetingDetail, MeetingRow, MemorySource, Segment, WebServer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// A daemon serving a small fixed library on an ephemeral loopback port.
pub struct Harness {
    pub addr: SocketAddr,
    /// The bearer token a legitimate client holds (ING-05).
    pub token: String,
    /// `127.0.0.1:<port>` — the only value `Host` may take.
    pub authority: String,
    /// `http://127.0.0.1:<port>` — the only value `Origin` may take.
    pub origin: String,
    /// The live state, so a test can mint a handoff token or publish a delta.
    pub state: AppState,
}

pub const MEETING_ID: &str = "01926f5a-0000-7000-8000-000000000001";
pub const MEETING_TITLE: &str = "Quarterly planning";
pub const SEGMENT_TEXT: &str = "we should ship the loopback guard first";
/// The user's own words. Distinct from the transcript so a test can tell
/// which of the two a response actually carried.
pub const NOTE_TEXT: &str = "decide on the rebinding guard before Thursday";

pub async fn start() -> Harness {
    let source = MemorySource::new().with_meeting(MeetingDetail {
        meeting: MeetingRow {
            id: MEETING_ID.to_owned(),
            title: MEETING_TITLE.to_owned(),
            started_at_ms: 1_754_900_000_000,
            duration_ms: Some(3_600_000),
            state: "ready".to_owned(),
        },
        summary_md: Some("## Decisions\n- ship the guard".to_owned()),
        note_md: Some(NOTE_TEXT.to_owned()),
        segments: vec![Segment {
            idx: 0,
            start_ms: 12_000,
            speaker: Some("S0".to_owned()),
            text: SEGMENT_TEXT.to_owned(),
        }],
    });

    let server = WebServer::bind(0, std::sync::Arc::new(source))
        .await
        .expect("bind loopback");
    let addr = server.addr();
    let state = server.state().clone();
    let token = state.policy().secret().expose_hex();
    let authority = state.policy().authority().to_owned();
    let origin = state.policy().origin().to_owned();

    tokio::spawn(async move {
        let _ = server.serve().await;
    });

    Harness {
        addr,
        token,
        authority,
        origin,
        state,
    }
}

impl Harness {
    /// `Host` and `Authorization` set correctly — what the SPA sends.
    pub fn authorised(&self) -> Vec<(String, String)> {
        vec![
            ("Host".into(), self.authority.clone()),
            ("Origin".into(), self.origin.clone()),
            ("Authorization".into(), format!("Bearer {}", self.token)),
        ]
    }

    /// `Host` correct, no credential.
    pub fn anonymous(&self) -> Vec<(String, String)> {
        vec![("Host".into(), self.authority.clone())]
    }

    pub async fn send(&self, request: &str) -> RawResponse {
        send(self.addr, request).await
    }

    pub async fn get(&self, path: &str, headers: &[(String, String)]) -> RawResponse {
        self.send(&build("GET", path, headers, None)).await
    }

    pub async fn post(
        &self,
        path: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> RawResponse {
        self.send(&build("POST", path, headers, body)).await
    }
}

/// Build a request literally, header by header, in the order given.
///
/// No normalisation, no de-duplication: a test that wants two `Host` headers
/// gets two `Host` headers.
pub fn build(method: &str, path: &str, headers: &[(String, String)], body: Option<&str>) -> String {
    let mut out = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    // `close` so the read side sees EOF and the test does not need a timeout.
    out.push_str("Connection: close\r\n");
    if let Some(body) = body {
        out.push_str("Content-Type: application/json\r\n");
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        out.push_str("\r\n");
        out.push_str(body);
    } else {
        out.push_str("\r\n");
    }
    out
}

pub async fn send(addr: SocketAddr, request: &str) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    RawResponse::parse(raw)
}

/// A response, kept as bytes as well as parsed, because ING-09 is a claim
/// about bytes.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub raw: Vec<u8>,
}

impl RawResponse {
    fn parse(raw: Vec<u8>) -> Self {
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let headers = lines
            .filter_map(|l| l.split_once(": "))
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_owned()))
            .collect();
        Self {
            status,
            headers,
            body: body.to_owned(),
            raw,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The response bytes with the `Date` header removed.
    ///
    /// `Date` is stamped per connection by `hyper` and is the one field two
    /// otherwise identical responses are allowed to differ in — it is also the
    /// one field that carries no information about *this* server. Everything
    /// else must match for ING-09 to hold.
    pub fn bytes_without_date(&self) -> Vec<u8> {
        let text = String::from_utf8_lossy(&self.raw);
        text.split("\r\n")
            .filter(|line| !line.to_ascii_lowercase().starts_with("date:"))
            .collect::<Vec<_>>()
            .join("\r\n")
            .into_bytes()
    }
}
