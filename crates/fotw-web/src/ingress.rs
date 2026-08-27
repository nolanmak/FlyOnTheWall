//! Loopback ingress control — docs/REQUIREMENTS.md 10.1, ING-01 … ING-09.
//!
//! # The threat this file exists for is DNS rebinding, not CSRF
//!
//! A page on `evil.test` cannot read a response from `http://127.0.0.1:51234`
//! — CORS stops it. So the attacker stops trying. They point `evil.test` at
//! their own server with a one-second TTL, serve a page, then re-answer the
//! next lookup for `evil.test` with `127.0.0.1`. The browser reconnects to
//! what it still believes is `evil.test`, and now the page is **same-origin
//! with our daemon**: `Sec-Fetch-Site: same-origin`, no preflight, arbitrary
//! request headers, and full read access to every response body.
//!
//! That defeats CORS, `SameSite=Strict` cookies and `tower_http`'s CSRF layer
//! alike, because all three are answers to *cross*-origin requests and this
//! request is not cross-origin any more. It is the bug class behind Ollama
//! CVE-2024-28224.
//!
//! Two things survive rebinding, and this module is both of them:
//!
//! 1. **The raw `Host` header** ([`IngressPolicy::check_host`]). The browser
//!    still sends `Host: evil.test` — script cannot set it, it is on the
//!    forbidden-header list — so an exact-match allowlist of
//!    `127.0.0.1:<port>` rejects the request no matter what the DNS says.
//! 2. **A secret the page cannot obtain** ([`IngressPolicy::check_bearer`]).
//!
//! # Why the browser is not going to help
//!
//! Chrome 142 put public→loopback fetches behind a permission prompt and 147
//! extended it to WebSockets. **Safari has shipped nothing** (WebKit
//! standards-positions #520 is still open), and macOS Local Network Privacy
//! explicitly exempts loopback *and* exempts WebKit. On the default browser of
//! the target platform there is zero OS-level and zero browser-level
//! protection. "It was blocked in Chrome" closes no ticket here.
//!
//! # Every refusal is the same bare 404
//!
//! ING-09. A `401 Unauthorized` with a `WWW-Authenticate` realm answers a
//! question the caller had no right to ask: *is FlyOnTheWall running, i.e. is
//! this person in a meeting right now?* A port-scanning page learns that from
//! the status code alone, without ever passing a single check. So
//! [`not_found`] is the only rejection this server has, it carries no body and
//! no headers, and it is the same value the router's fallback returns for a
//! path that does not exist. `tests/ingress.rs` compares the two responses
//! byte for byte.

use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::secret::Secret;
use crate::state::AppState;

/// Why a request was refused.
///
/// Never reaches the client — every variant renders as the same [`not_found`].
/// It exists so the daemon can count refusals locally and so the tests can
/// assert *which* control fired, which is the difference between a test that
/// proves the `Host` allowlist works and one that merely proves something
/// somewhere returned 404.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deny {
    /// ING-01: the connection did not come from the loopback interface.
    PeerNotLoopback,
    /// ING-02: no `Host` header and no `:authority`.
    HostMissing,
    /// ING-02: more than one `Host`, or a `Host` that disagrees with the
    /// request-target's authority.
    HostAmbiguous,
    /// ING-02: a `Host` that is not on the allowlist — the rebinding signal.
    HostNotAllowed,
    /// ING-04: an `Origin` that is present and not ours (including `null`).
    OriginNotAllowed,
    /// ING-05: no `Authorization: Bearer`.
    TokenMissing,
    /// ING-05: a bearer token that is not the secret.
    TokenInvalid,
    /// ING-07: a WS ticket that was missing, expired, already spent or wrong.
    TicketInvalid,
}

/// The allowlists and the secret, fixed for the lifetime of one daemon start.
#[derive(Debug)]
pub struct IngressPolicy {
    authorities: Vec<String>,
    origins: Vec<String>,
    secret: Secret,
}

impl IngressPolicy {
    /// The policy for a server bound to `127.0.0.1:port`, with a fresh secret.
    ///
    /// Exactly one authority and exactly one origin. Not `localhost:<port>`:
    /// the launch URL is written with the literal address (ING-01), so nothing
    /// legitimate ever sends the name, and admitting a name means admitting
    /// whatever a resolver decides that name means today.
    #[must_use]
    pub fn for_loopback_port(port: u16) -> Self {
        Self {
            authorities: vec![format!("127.0.0.1:{port}")],
            origins: vec![format!("http://127.0.0.1:{port}")],
            secret: Secret::generate(),
        }
    }

    /// The bearer token this server will accept.
    #[must_use]
    pub fn secret(&self) -> &Secret {
        &self.secret
    }

    /// The single origin the SPA runs on — the `connect-src` of the CSP and
    /// the value `check_origin` demands.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origins[0]
    }

    /// The `host:port` the SPA must be reached at.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authorities[0]
    }

    /// ING-01. A tripwire, not the control: the control is binding the literal
    /// [`std::net::Ipv4Addr::LOCALHOST`]. This catches the day someone
    /// "temporarily" changes that bind address to `0.0.0.0` and the LAN can
    /// suddenly reach the daemon — the listener would still be listening, but
    /// every request would 404.
    ///
    /// # Errors
    ///
    /// [`Deny::PeerNotLoopback`] for any non-loopback peer.
    pub fn check_peer(&self, peer: IpAddr) -> Result<(), Deny> {
        if peer.is_loopback() {
            Ok(())
        } else {
            Err(Deny::PeerNotLoopback)
        }
    }

    /// ING-02 and ING-03: the raw `Host`, exact-matched.
    ///
    /// # Why this reads the header map itself
    ///
    /// **`axum_extra::extract::Host` is unusable here.** It returns the first
    /// of `Forwarded`, `X-Forwarded-Host`, then the real `Host` — and the
    /// first two are ordinary headers that a same-origin (i.e. rebound) page
    /// sets freely. An attacker sends `X-Forwarded-Host: 127.0.0.1:51234`
    /// along with `Host: evil.test` and the check passes. It compiles, it
    /// reads correctly, its tests are green, and it stops nothing. (It is also
    /// `#[deprecated]` upstream for a related reason.) That is ING-03: a
    /// rebinding check that does nothing is worse than none, because it ends
    /// the conversation.
    ///
    /// Two `Host` headers are a refusal rather than a "take the first": which
    /// one a proxy or a parser honours is exactly the disagreement request
    /// smuggling is made of, and there is no legitimate sender of two.
    ///
    /// # Errors
    ///
    /// [`Deny::HostMissing`], [`Deny::HostAmbiguous`] or
    /// [`Deny::HostNotAllowed`].
    pub fn check_host(&self, headers: &HeaderMap, uri: &Uri) -> Result<(), Deny> {
        let mut host_values = headers.get_all(header::HOST).iter();
        let host = host_values.next();
        if host_values.next().is_some() {
            return Err(Deny::HostAmbiguous);
        }

        // HTTP/2 has no `Host`; the authority is a pseudo-header, which hyper
        // surfaces as the request-target authority. HTTP/1.1 absolute-form
        // request targets land in the same place, and there they must agree
        // with `Host` or the request is ambiguous.
        let authority = uri.authority().map(|a| a.as_str().to_ascii_lowercase());
        let host = match host {
            Some(value) => {
                let value = value.to_str().map_err(|_| Deny::HostNotAllowed)?;
                let value = value.trim().to_ascii_lowercase();
                if authority.as_ref().is_some_and(|a| *a != value) {
                    return Err(Deny::HostAmbiguous);
                }
                value
            }
            None => authority.ok_or(Deny::HostMissing)?,
        };

        // Whole-string equality. Never `ends_with`, never `starts_with`, never
        // `contains` on the *value*: `127.0.0.1:51234.evil.test` ends with
        // nothing useful but `evil.test:51234` would pass a suffix test on the
        // port, and `127.0.0.1.evil.test` passes a prefix test. Each of those
        // is a name an attacker can register.
        if self.authorities.contains(&host) {
            Ok(())
        } else {
            Err(Deny::HostNotAllowed)
        }
    }

    /// ING-04: an `Origin`, when present, must be ours.
    ///
    /// Absent is permitted deliberately — `curl`, the `fotw` CLI and the
    /// daemon's own health probes send none, and demanding one would make the
    /// bearer token useless outside a browser without buying anything: a page
    /// cannot suppress `Origin` on a cross-origin request, so "absent" is
    /// never a browser attacker.
    ///
    /// `Origin: null` is refused explicitly. It is what a sandboxed iframe, a
    /// `data:` URL and some redirect chains send, and treating it as "no
    /// origin" turns a hostile frame into a trusted caller.
    ///
    /// # Errors
    ///
    /// [`Deny::OriginNotAllowed`].
    pub fn check_origin(&self, headers: &HeaderMap) -> Result<(), Deny> {
        let mut values = headers.get_all(header::ORIGIN).iter();
        let Some(origin) = values.next() else {
            return Ok(());
        };
        if values.next().is_some() {
            return Err(Deny::OriginNotAllowed);
        }
        let origin = origin
            .to_str()
            .map_err(|_| Deny::OriginNotAllowed)?
            .trim()
            .to_ascii_lowercase();
        if self.origins.contains(&origin) {
            Ok(())
        } else {
            Err(Deny::OriginNotAllowed)
        }
    }

    /// ING-05: `Authorization: Bearer <64 hex>`, compared in constant time.
    ///
    /// The scheme is matched case-insensitively (RFC 7235 says it is
    /// case-insensitive) but the token is not: it is hex we minted.
    ///
    /// # Errors
    ///
    /// [`Deny::TokenMissing`] when there is no usable header at all,
    /// [`Deny::TokenInvalid`] when there is one and it is wrong.
    pub fn check_bearer(&self, headers: &HeaderMap) -> Result<(), Deny> {
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let value = values.next().ok_or(Deny::TokenMissing)?;
        if values.next().is_some() {
            return Err(Deny::TokenInvalid);
        }
        let value = value.to_str().map_err(|_| Deny::TokenInvalid)?;
        let (scheme, token) = value.split_once(' ').ok_or(Deny::TokenMissing)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(Deny::TokenMissing);
        }
        if self.secret.matches(token.trim().as_bytes()) {
            Ok(())
        } else {
            Err(Deny::TokenInvalid)
        }
    }
}

/// Paths reachable without a bearer token, and why each one has to be.
///
/// * `/` and `/assets/*` — the SPA shell. The browser arrives holding the
///   handoff token and nothing else; it cannot present a bearer it has not
///   been given yet. The shell contains no meeting data, which is what makes
///   this safe: it is HTML, CSS and JS, and the `Host` and `Origin` checks
///   still apply to it.
/// * `/api/handoff` — the exchange that *hands out* the bearer. It is
///   protected by the one-time ≤30 s handoff token instead (ING-10).
/// * `/api/stream` — a browser cannot set `Authorization` on a WebSocket
///   handshake. Not "it is awkward": `new WebSocket()` takes a URL and a
///   subprotocol list, and that is the entire API. So the stream is
///   authenticated by a single-use ticket instead (ING-07), checked inside the
///   handler.
///
/// Anything not listed here needs the bearer. `tests/ingress.rs` walks every
/// route and asserts exactly this split, so adding an endpoint without
/// thinking about it fails a test rather than shipping open.
fn bearer_exempt(path: &str) -> bool {
    matches!(path, "/" | "/api/handoff" | "/api/stream") || path.starts_with("/assets/")
}

/// The one refusal this server has: ING-09.
///
/// No body, no `WWW-Authenticate`, no `Content-Type`, no hint. Identical to
/// what [`crate::server::router`]'s fallback returns for a path that does not
/// exist, so a scanner cannot even distinguish "wrong token" from "wrong
/// server".
#[must_use]
pub fn not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .expect("a bare 404 is always constructible")
}

/// The guard, as an `axum` layer, so no route can forget it.
///
/// Runs outermost — before routing, before every extractor — for two reasons.
/// A per-handler check is one `#[allow]` or one copy-paste away from being
/// absent on the newest endpoint. And extractor *rejections* are themselves an
/// oracle: `Json`'s rejection body says "Expected request with
/// `Content-Type: application/json`", which tells an unauthenticated caller
/// that the path exists and takes JSON. Refusing before any of that runs means
/// an unauthorised caller learns nothing but "404".
///
/// The checks are ordered cheapest-first, and none of them touches the
/// database, the filesystem or the network — so the "no differential latency"
/// half of ING-09 falls out of the structure: every refusal returns after at
/// most a few header lookups and one constant-time comparison.
///
/// # The 404 normaliser on the way out
///
/// This layer also rewrites **every** 404 that comes back through it, because
/// uniformity is otherwise only as good as the last person who added a route.
/// The concrete case that made this necessary: `axum` appends an `Allow`
/// header to the response from `method_not_allowed_fallback`, so `POST
/// /api/meetings` returned a 404 carrying `allow: GET,HEAD` — which tells an
/// unauthenticated caller that the path exists and takes `GET`, the exact fact
/// ING-09 is written to withhold. Normalising here means that stays fixed no
/// matter what a future layer decides to attach.
pub async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Err(_deny) = check(&state, &req) {
        return not_found();
    }
    let response = next.run(req).await;
    if response.status() == StatusCode::NOT_FOUND {
        return not_found();
    }
    response
}

fn check(state: &AppState, req: &Request) -> Result<(), Deny> {
    let policy = state.policy();

    // Read `ConnectInfo` out of the extensions rather than extracting it, so
    // that a router driven directly by `tower::ServiceExt::oneshot` (which has
    // no socket and therefore no peer) does not fail on an extractor rejection
    // it can never satisfy. `crate::server::serve` always installs it.
    if let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        policy.check_peer(peer.ip())?;
    }
    policy.check_host(req.headers(), req.uri())?;
    policy.check_origin(req.headers())?;
    if !bearer_exempt(req.uri().path()) {
        policy.check_bearer(req.headers())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn policy() -> IngressPolicy {
        IngressPolicy::for_loopback_port(51234)
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    const PATH: &str = "/api/meetings";

    fn uri() -> Uri {
        PATH.parse().unwrap()
    }

    #[test]
    fn the_real_host_is_accepted() {
        let p = policy();
        assert_eq!(
            p.check_host(&headers(&[("host", "127.0.0.1:51234")]), &uri()),
            Ok(())
        );
    }

    /// The rebinding request itself. The DNS says `127.0.0.1`; the `Host` still
    /// says `evil.test`, because script cannot change it.
    #[test]
    fn a_rebound_host_is_refused() {
        let p = policy();
        for host in [
            "evil.test",
            "evil.test:51234",
            "127.0.0.1.evil.test:51234",
            "127.0.0.1:51234.evil.test",
            "attacker.test:51234",
        ] {
            assert_eq!(
                p.check_host(&headers(&[("host", host)]), &uri()),
                Err(Deny::HostNotAllowed),
                "{host} must be refused"
            );
        }
    }

    /// ING-03, as an executable claim. These are the headers
    /// `axum_extra::extract::Host` would have preferred over the real one; the
    /// request must still be refused on the strength of `Host` alone.
    #[test]
    fn forwarded_headers_cannot_override_the_host() {
        let p = policy();
        for spoof in [
            ("forwarded", "host=127.0.0.1:51234"),
            ("x-forwarded-host", "127.0.0.1:51234"),
            ("x-forwarded-server", "127.0.0.1:51234"),
            ("x-host", "127.0.0.1:51234"),
        ] {
            let h = headers(&[("host", "evil.test"), spoof]);
            assert_eq!(
                p.check_host(&h, &uri()),
                Err(Deny::HostNotAllowed),
                "{} must not be able to override Host",
                spoof.0
            );
        }
    }

    #[test]
    fn a_wrong_port_is_a_different_server() {
        let p = policy();
        // Another loopback service on another port is not us. Cookie scoping
        // gets this wrong (ING-08); the authority allowlist does not.
        assert_eq!(
            p.check_host(&headers(&[("host", "127.0.0.1:51235")]), &uri()),
            Err(Deny::HostNotAllowed)
        );
        assert_eq!(
            p.check_host(&headers(&[("host", "127.0.0.1")]), &uri()),
            Err(Deny::HostNotAllowed)
        );
    }

    #[test]
    fn localhost_the_name_is_not_on_the_allowlist() {
        let p = policy();
        assert_eq!(
            p.check_host(&headers(&[("host", "localhost:51234")]), &uri()),
            Err(Deny::HostNotAllowed)
        );
    }

    #[test]
    fn two_host_headers_are_refused_rather_than_resolved() {
        let p = policy();
        let h = headers(&[("host", "127.0.0.1:51234"), ("host", "evil.test")]);
        assert_eq!(p.check_host(&h, &uri()), Err(Deny::HostAmbiguous));
        let h = headers(&[("host", "evil.test"), ("host", "127.0.0.1:51234")]);
        assert_eq!(p.check_host(&h, &uri()), Err(Deny::HostAmbiguous));
    }

    #[test]
    fn a_missing_host_is_refused() {
        let p = policy();
        assert_eq!(
            p.check_host(&HeaderMap::new(), &uri()),
            Err(Deny::HostMissing)
        );
    }

    #[test]
    fn an_absolute_form_target_must_agree_with_host() {
        let p = policy();
        let good: Uri = "http://127.0.0.1:51234/api/meetings".parse().unwrap();
        assert_eq!(
            p.check_host(&headers(&[("host", "127.0.0.1:51234")]), &good),
            Ok(())
        );
        // The classic split: an allowed authority in the request line, a
        // hostile one in `Host`. Whichever half a downstream reader trusts,
        // this server refuses.
        assert_eq!(
            p.check_host(&headers(&[("host", "evil.test")]), &good),
            Err(Deny::HostAmbiguous)
        );
        let bad: Uri = "http://evil.test/api/meetings".parse().unwrap();
        assert_eq!(
            p.check_host(&HeaderMap::new(), &bad),
            Err(Deny::HostNotAllowed)
        );
    }

    #[test]
    fn host_matching_ignores_case_and_surrounding_space() {
        let p = policy();
        assert_eq!(
            p.check_host(&headers(&[("host", " 127.0.0.1:51234 ")]), &uri()),
            Ok(())
        );
    }

    #[test]
    fn our_own_origin_is_accepted_and_no_origin_is_permitted() {
        let p = policy();
        assert_eq!(p.check_origin(&HeaderMap::new()), Ok(()));
        assert_eq!(
            p.check_origin(&headers(&[("origin", "http://127.0.0.1:51234")])),
            Ok(())
        );
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        let p = policy();
        for origin in [
            "http://evil.test",
            "https://127.0.0.1:51234",
            "http://127.0.0.1:51235",
            "http://localhost:51234",
            "null",
            "http://127.0.0.1:51234.evil.test",
        ] {
            assert_eq!(
                p.check_origin(&headers(&[("origin", origin)])),
                Err(Deny::OriginNotAllowed),
                "{origin} must be refused"
            );
        }
    }

    #[test]
    fn the_secret_is_required_and_sufficient() {
        let p = policy();
        let token = p.secret().expose_hex();
        assert_eq!(
            p.check_bearer(&headers(&[("authorization", &format!("Bearer {token}"))])),
            Ok(())
        );
        assert_eq!(
            p.check_bearer(&headers(&[("authorization", &format!("bearer {token}"))])),
            Ok(())
        );
        assert_eq!(p.check_bearer(&HeaderMap::new()), Err(Deny::TokenMissing));
        assert_eq!(
            p.check_bearer(&headers(&[("authorization", "Bearer wrong")])),
            Err(Deny::TokenInvalid)
        );
        assert_eq!(
            p.check_bearer(&headers(&[("authorization", &format!("Basic {token}"))])),
            Err(Deny::TokenMissing)
        );
        // A prefix of the real token is not the token.
        assert_eq!(
            p.check_bearer(&headers(&[(
                "authorization",
                &format!("Bearer {}", &token[..token.len() - 1])
            )])),
            Err(Deny::TokenInvalid)
        );
    }

    #[test]
    fn two_daemons_do_not_share_a_secret() {
        let a = IngressPolicy::for_loopback_port(51234);
        let b = IngressPolicy::for_loopback_port(51234);
        let token = a.secret().expose_hex();
        assert_eq!(
            b.check_bearer(&headers(&[("authorization", &format!("Bearer {token}"))])),
            Err(Deny::TokenInvalid)
        );
    }

    #[test]
    fn only_loopback_peers_pass_the_tripwire() {
        let p = policy();
        assert_eq!(p.check_peer("127.0.0.1".parse().unwrap()), Ok(()));
        assert_eq!(p.check_peer("::1".parse().unwrap()), Ok(()));
        for addr in ["192.168.1.44", "10.0.0.7", "203.0.113.9", "0.0.0.0"] {
            assert_eq!(
                p.check_peer(addr.parse().unwrap()),
                Err(Deny::PeerNotLoopback),
                "{addr} must be refused"
            );
        }
    }

    /// ING-03 as a structural claim rather than a promise in a comment. The
    /// extractor cannot be misused if it is not in the dependency graph, and
    /// the failure mode it produces — a `Host` check that reads
    /// `X-Forwarded-Host` — is invisible in review and green in tests.
    #[test]
    fn this_crate_does_not_depend_on_axum_extra() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            !manifest.contains("axum-extra") && !manifest.contains("axum_extra"),
            "ING-03: `axum_extra::extract::Host` prefers Forwarded and \
             X-Forwarded-Host over the real header, which is a complete \
             bypass of ING-02 under rebinding"
        );
    }

    /// ING-09's response, inspected directly. No body, and no headers at all —
    /// not `WWW-Authenticate`, not `Content-Type`, not a custom error code.
    #[test]
    fn the_only_refusal_carries_nothing() {
        let response = not_found();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response.headers().is_empty(),
            "a refusal must say nothing at all, got {:?}",
            response.headers()
        );
    }

    #[test]
    fn the_bearer_exempt_list_is_exactly_the_three_documented_cases() {
        assert!(bearer_exempt("/"));
        assert!(bearer_exempt("/assets/app.js"));
        assert!(bearer_exempt("/api/handoff"));
        assert!(bearer_exempt("/api/stream"));
        for path in [
            "/api/meetings",
            "/api/meetings/abc",
            "/api/search",
            "/api/ws-ticket",
            "/api/health",
            "/api/handoff/../meetings",
            "/assets",
        ] {
            assert!(!bearer_exempt(path), "{path} must require the bearer");
        }
    }
}
