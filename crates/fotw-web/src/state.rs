//! What every handler is given: the policy, the library, the token tables and
//! the delta hub.
//!
//! One `Arc` rather than four in the router's state, so that adding a control
//! does not mean touching every handler signature — and so that
//! [`AppState::policy`] is the only way to reach the secret, which keeps the
//! set of places that can compare it to one.

use std::sync::{Arc, OnceLock};

use crate::github::GithubExport;
use crate::health::DaemonHealth;
use crate::ingress::IngressPolicy;
use crate::recorder::RecorderControl;
use crate::source::MeetingSource;
use crate::stream::DeltaHub;
use crate::summarize::SummarizeControl;
use crate::tokens::{HANDOFF_TTL, TokenTable, WS_TICKET_TTL};

/// Shared, cheap to clone, immutable except for the token tables and the hub.
#[derive(Clone, Debug)]
pub struct AppState {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    policy: IngressPolicy,
    source: Arc<dyn MeetingSource>,
    tickets: TokenTable,
    handoff: TokenTable,
    hub: Arc<DeltaHub>,
    csp: String,
    recorder: Option<Arc<dyn RecorderControl>>,
    github: Option<Arc<dyn GithubExport>>,
    summarize: Option<Arc<dyn SummarizeControl>>,
    /// Bound after construction, unlike every control above it — see
    /// [`AppState::set_health`].
    health: OnceLock<Arc<dyn DaemonHealth>>,
}

impl std::fmt::Debug for dyn RecorderControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Same reason as the library below: a `{:?}` on the app state must not
        // reach anything that knows what is being recorded.
        f.write_str("RecorderControl(<redacted>)")
    }
}

impl std::fmt::Debug for dyn GithubExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // It holds the library handle the pusher exports transcripts from.
        f.write_str("GithubExport(<redacted>)")
    }
}

impl std::fmt::Debug for dyn SummarizeControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // It holds the library handle and reads the keychain.
        f.write_str("SummarizeControl(<redacted>)")
    }
}

impl std::fmt::Debug for dyn DaemonHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // It holds a library handle of its own, for the queue depth.
        f.write_str("DaemonHealth(<redacted>)")
    }
}

impl std::fmt::Debug for dyn MeetingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The library is the thing §10 says must be unreachable from the
        // logging subsystem. A `{:?}` on the app state prints this.
        f.write_str("MeetingSource(<redacted>)")
    }
}

impl AppState {
    /// Assemble the state for a server whose policy is already fixed to a
    /// port.
    #[must_use]
    pub fn new(policy: IngressPolicy, source: Arc<dyn MeetingSource>) -> Self {
        Self::with_recorder(policy, source, None)
    }

    /// [`AppState::new`], with a recorder the UI may drive.
    ///
    /// Separate constructor rather than a wider `new` because `new` has call
    /// sites that have no recorder and should not grow a `None` — the
    /// read-only preview server among them. `AppState` wraps an `Arc`, so this
    /// cannot be a post-construction builder without unwrapping it.
    #[must_use]
    pub fn with_recorder(
        policy: IngressPolicy,
        source: Arc<dyn MeetingSource>,
        recorder: Option<Arc<dyn RecorderControl>>,
    ) -> Self {
        Self::with_controls(policy, source, recorder, None)
    }

    /// [`AppState::with_recorder`], with the GitHub export control as well.
    ///
    /// Grown the same way `with_recorder` grew out of `new`: existing call
    /// sites keep the arity that says what they actually have.
    #[must_use]
    pub fn with_controls(
        policy: IngressPolicy,
        source: Arc<dyn MeetingSource>,
        recorder: Option<Arc<dyn RecorderControl>>,
        github: Option<Arc<dyn GithubExport>>,
    ) -> Self {
        Self::with_all_controls(policy, source, recorder, github, None)
    }

    /// [`AppState::with_controls`], with the summarize-engine control as well
    /// (issue #74).
    ///
    /// Grown the same way the two before it grew: existing call sites keep the
    /// arity that says what they actually have, rather than every one of them
    /// gaining a `None`.
    #[must_use]
    pub fn with_all_controls(
        policy: IngressPolicy,
        source: Arc<dyn MeetingSource>,
        recorder: Option<Arc<dyn RecorderControl>>,
        github: Option<Arc<dyn GithubExport>>,
        summarize: Option<Arc<dyn SummarizeControl>>,
    ) -> Self {
        let csp = content_security_policy(policy.origin());
        Self {
            inner: Arc::new(Inner {
                policy,
                source,
                recorder,
                github,
                summarize,
                health: OnceLock::new(),
                tickets: TokenTable::new(WS_TICKET_TTL),
                handoff: TokenTable::new(HANDOFF_TTL),
                hub: Arc::new(DeltaHub::new()),
                csp,
            }),
        }
    }

    /// The ingress allowlists and the per-start secret.
    #[must_use]
    pub fn policy(&self) -> &IngressPolicy {
        &self.inner.policy
    }

    /// The recorder, if this daemon has one.
    ///
    /// `None` on a read-only server. The handlers answer a bare 404 in that
    /// case, so a daemon that cannot record is indistinguishable from one that
    /// has never heard of the route.
    #[must_use]
    pub fn recorder(&self) -> Option<Arc<dyn RecorderControl>> {
        self.inner.recorder.clone()
    }

    /// The GitHub export control, if this daemon has one.
    ///
    /// `None` on a read-only server, and the handlers answer a bare 404 —
    /// a build that cannot push is indistinguishable from one that has never
    /// heard of the route.
    #[must_use]
    pub fn github(&self) -> Option<Arc<dyn GithubExport>> {
        self.inner.github.clone()
    }

    /// The summarize-engine control, if this daemon has one.
    ///
    /// `None` on a read-only server, and the handlers answer a bare 404 — a
    /// build that cannot summarise is indistinguishable from one that has
    /// never heard of the route (ING-09).
    #[must_use]
    pub fn summarize(&self) -> Option<Arc<dyn SummarizeControl>> {
        self.inner.summarize.clone()
    }

    /// The daemon's health surface, if this daemon has one (#101).
    ///
    /// `None` on a read-only server, and the handler answers a bare 404 —
    /// a build with nothing to report is indistinguishable from one that has
    /// never heard of the route (ING-09).
    #[must_use]
    pub fn health(&self) -> Option<Arc<dyn DaemonHealth>> {
        self.inner.health.get().map(Arc::clone)
    }

    /// Bind the health surface, once.
    ///
    /// # Why this one is set rather than constructed
    ///
    /// Every control above it is an argument to a constructor, and the comment
    /// on `with_recorder` explains why: `AppState` wraps an `Arc`, so a
    /// builder would have to unwrap it. This one is different in the way that
    /// matters — what it reports includes the port that was bound and the path
    /// of the log the daemon opened, and neither exists until `bind` has
    /// returned. `serve.rs` late-binds the delta hub through a `OnceLock` for
    /// exactly the same reason.
    ///
    /// The window before it is set cannot be occupied: `bind` returns a
    /// listener that is not being served yet, and `serve()` is called after.
    /// A second call is ignored rather than panicking — a health surface is
    /// not worth taking a daemon down over.
    pub fn set_health(&self, health: Arc<dyn DaemonHealth>) {
        let _ = self.inner.health.set(health);
    }

    /// The library, for [`tokio::task::spawn_blocking`].
    #[must_use]
    pub fn source(&self) -> Arc<dyn MeetingSource> {
        Arc::clone(&self.inner.source)
    }

    /// ING-07's single-use WebSocket tickets.
    #[must_use]
    pub fn tickets(&self) -> &TokenTable {
        &self.inner.tickets
    }

    /// ING-10's single-use launch handoff tokens.
    #[must_use]
    pub fn handoff(&self) -> &TokenTable {
        &self.inner.handoff
    }

    /// The 10 Hz transcript fan-out (§5.5).
    #[must_use]
    pub fn hub(&self) -> &Arc<DeltaHub> {
        &self.inner.hub
    }

    /// The `Content-Security-Policy` served with the SPA shell (ING-11).
    #[must_use]
    pub fn csp(&self) -> &str {
        &self.inner.csp
    }

    /// Mint a handoff token and return the URL the daemon should open.
    ///
    /// ING-10: this URL ends up in `open(1)`'s argv and in the browser's
    /// synced history, so the only secret in it is worth one redemption inside
    /// thirty seconds. The bearer token itself never appears here.
    #[must_use]
    pub fn launch_url(&self) -> String {
        let token = self.handoff().mint();
        format!("{}/?t={token}", self.policy().origin())
    }
}

/// ING-11's Content-Security-Policy.
///
/// The reason a *local* app needs one: transcripts are attacker-influenced
/// text. Anyone in the meeting can say "script alert 1", a calendar
/// description can carry raw markup, and both flow into the same DOM as the
/// UI. The renderer uses `textContent` throughout, and this header is what
/// catches the day it does not.
///
/// * `default-src 'none'` — deny by default, then name what is allowed.
/// * `script-src 'self'` with **no** `'unsafe-inline'`: this is why the bearer
///   token is fetched by the SPA rather than injected into the shell as an
///   inline `<script>`, which would have forced a nonce or a hash and made the
///   strongest clause in the policy conditional.
/// * `connect-src` names the WebSocket origin explicitly. CSP3 says `'self'`
///   covers a `ws:` URL on the same host and port, but Safari's support for
///   that clause is exactly the kind of thing §10.1 says not to assume.
/// * `frame-ancestors 'none'` — a page that frames the UI cannot read it
///   cross-origin anyway, but it can clickjack it.
/// * `require-trusted-types-for 'script'` — enforced in Chromium, ignored
///   elsewhere; it turns "someone reintroduced `innerHTML`" from a
///   vulnerability into a console error.
#[must_use]
pub fn content_security_policy(origin: &str) -> String {
    let ws_origin = origin.replacen("http://", "ws://", 1);
    format!(
        "default-src 'none'; \
         script-src 'self'; \
         style-src 'self'; \
         img-src 'self' data:; \
         font-src 'self'; \
         connect-src 'self' {ws_origin}; \
         base-uri 'none'; \
         form-action 'none'; \
         frame-ancestors 'none'; \
         object-src 'none'; \
         require-trusted-types-for 'script'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;

    fn state() -> AppState {
        AppState::new(
            IngressPolicy::for_loopback_port(51234),
            Arc::new(MemorySource::new()),
        )
    }

    #[test]
    fn the_launch_url_carries_a_one_time_token_and_nothing_else() {
        let s = state();
        let url = s.launch_url();
        assert!(url.starts_with("http://127.0.0.1:51234/?t="));
        assert!(
            !url.contains(&s.policy().secret().expose_hex()),
            "ING-10: the bearer token must never be in a URL — `open(1)` puts \
             it in argv and in synced browser history"
        );
        let token = url.split_once("?t=").unwrap().1.to_owned();
        assert!(s.handoff().redeem(&token));
        assert!(!s.handoff().redeem(&token), "burned on redemption");
    }

    #[test]
    fn the_csp_denies_by_default_and_allows_no_inline_script() {
        let csp = state().csp().to_owned();
        assert!(csp.starts_with("default-src 'none'"));
        assert!(csp.contains("script-src 'self'"));
        assert!(!csp.contains("unsafe-inline"));
        assert!(!csp.contains("unsafe-eval"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("object-src 'none'"));
        assert!(csp.contains("connect-src 'self' ws://127.0.0.1:51234"));
    }

    #[test]
    fn debug_on_the_state_prints_neither_the_secret_nor_the_library() {
        let s = state();
        let printed = format!("{s:?}");
        assert!(!printed.contains(&s.policy().secret().expose_hex()));
        assert!(printed.contains("redacted"));
    }
}
