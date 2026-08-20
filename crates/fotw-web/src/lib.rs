//! `fotw-web` — the daemon's loopback HTTP/WebSocket server and its SPA.
//!
//! This crate is mostly a security control with an API attached. The API is
//! five endpoints over data `fotw-store` already knows how to fetch; the
//! security control is docs/REQUIREMENTS.md **§10.1**, and it is the reason
//! the crate exists as its own compilation unit with its own test suite.
//!
//! # The problem, stated the way it actually is
//!
//! `fotwd` holds every transcript the user has ever recorded and can read
//! keychain-backed API keys. It listens on `127.0.0.1`. **Any web page the
//! user visits can attempt requests to `127.0.0.1`** — no permission, no
//! prompt, no user gesture.
//!
//! The instinct is that CORS handles this. It does not, because the attack is
//! not cross-origin. **DNS rebinding makes the attacker same-origin**: they
//! serve a page from `evil.test` with a one-second TTL, then re-answer the
//! next lookup with `127.0.0.1`. The browser now believes our daemon *is*
//! `evil.test`. `Sec-Fetch-Site: same-origin`, no preflight, arbitrary request
//! headers, full response reads. CORS, `SameSite=Strict` and
//! `tower_http::csrf` are all answers to a question that is no longer being
//! asked. This is the bug class behind Ollama CVE-2024-28224.
//!
//! And the browser will not save us. Chrome 142 gated public→loopback fetch
//! behind a prompt and 147 extended it to WebSockets, but **Safari — the
//! default browser of the target platform — has shipped nothing**, and macOS
//! Local Network Privacy explicitly exempts loopback *and* WebKit. §10.1 is
//! blunt about the consequence: do the adversarial testing in Safari, and "it
//! was blocked in Chrome" closes no ticket.
//!
//! # The twelve controls, and where each one lives
//!
//! | ID | Control | Here |
//! |---|---|---|
//! | ING-01 | bind literal `127.0.0.1`, peer `is_loopback()` tripwire | [`server::WebServer::bind`], [`ingress::IngressPolicy::check_peer`] |
//! | ING-02 | raw `Host` allow-list, exact match | [`ingress::IngressPolicy::check_host`] |
//! | ING-03 | never `axum_extra::extract::Host` | ditto — this crate does not depend on `axum-extra` at all |
//! | ING-04 | `Origin` allow-list when present | [`ingress::IngressPolicy::check_origin`] |
//! | ING-05 | 256-bit per-start secret, constant-time compare | [`secret::Secret`] |
//! | ING-06 | explicit `Origin` check before `on_upgrade` | [`stream::authorize_upgrade`] |
//! | ING-07 | single-use ≤10 s WebSocket ticket | [`tokens::TokenTable`], [`api::ws_ticket`] |
//! | ING-08 | no cookies, ever | nothing in this crate sets `Set-Cookie`; `tests/ingress.rs` asserts it |
//! | ING-09 | uniform bare 404 | [`ingress::not_found`] |
//! | ING-10 | one-time ≤30 s handoff token in the launch URL | [`state::AppState::launch_url`], [`api::handoff`] |
//! | ING-11 | strict CSP on the shell | [`state::content_security_policy`] |
//! | ING-12 | state file `0600` in a `0700` dir, temp + `rename(2)` | [`state_file`] |
//!
//! **Explicitly out of scope, per §10.1:** same-user local malware. It can
//! read the SQLCipher database directly. Writing that down is what keeps the
//! threat model finite.
//!
//! # Using it
//!
//! ```no_run
//! use std::sync::Arc;
//! use fotw_web::{MemorySource, WebServer};
//!
//! # async fn run() -> std::io::Result<()> {
//! let server = WebServer::bind(0, Arc::new(MemorySource::new())).await?;
//! // The URL to hand to `open(1)`. It carries a one-time token, never the
//! // session secret (ING-10).
//! let url = server.state().launch_url();
//! // The pipeline publishes into this; a 10 Hz tick batches it (§5.5).
//! let hub = Arc::clone(server.state().hub());
//! hub.spawn_flusher();
//! server.serve().await
//! # }
//! ```

#![warn(missing_docs)]

pub mod api;
pub mod assets;
pub mod ingress;
pub mod query;
pub mod recorder;
pub mod secret;
pub mod server;
pub mod source;
pub mod state;
pub mod state_file;
pub mod stream;
pub mod tokens;

#[cfg(feature = "store")]
pub mod store_source;

pub use crate::ingress::{Deny, IngressPolicy, not_found};
pub use crate::recorder::{RecorderControl, RecorderError, RecordingState, RecordingStatus};
pub use crate::secret::Secret;
pub use crate::server::{WebServer, router};
pub use crate::source::{
    Hit, MeetingDetail, MeetingRow, MeetingSource, MemorySource, Segment, SourceError,
};
pub use crate::state::AppState;
pub use crate::state_file::{DaemonState, read_state_file, write_state_file};
pub use crate::stream::{Delta, DeltaHub, FLUSH_INTERVAL, Frame};
pub use crate::tokens::{HANDOFF_TTL, TokenTable, WS_TICKET_TTL};

#[cfg(feature = "store")]
pub use crate::store_source::StoreSource;
