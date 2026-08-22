//! Binding, routing and serving.
//!
//! # ING-01, and the string that must never appear here
//!
//! The listener binds [`Ipv4Addr::LOCALHOST`] — the literal `127.0.0.1`, as a
//! value. Not `"localhost:0"`, not `"localhost"`, not any string that a
//! resolver gets a say in. On a machine whose `/etc/hosts` maps `localhost` to
//! `::1` first, or whose resolver has been persuaded to map it somewhere else,
//! a string bind lands on an address the operator did not choose — and the one
//! thing a loopback-only daemon cannot afford is to be surprised about which
//! interface it is on. [`WebServer::bind`] additionally *verifies* the address
//! it actually got and refuses to serve from anything else, so a future edit
//! to the bind expression fails at startup instead of quietly listening on the
//! LAN.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tokio::net::TcpListener;

use crate::ingress::{IngressPolicy, guard, not_found};
use crate::source::MeetingSource;
use crate::state::AppState;
use crate::{api, assets, stream};

/// A bound, not-yet-serving loopback HTTP server.
#[derive(Debug)]
pub struct WebServer {
    listener: TcpListener,
    addr: SocketAddr,
    state: AppState,
}

impl WebServer {
    /// Bind `127.0.0.1:port`. Pass `0` for an ephemeral port.
    ///
    /// `EADDRINUSE` from a fixed port is not an error to paper over — §5.5
    /// makes it the daemon's single-instance mutex.
    ///
    /// # Errors
    ///
    /// The bind failed, or — see the module docs — it succeeded on something
    /// that is not loopback.
    pub async fn bind(port: u16, source: Arc<dyn MeetingSource>) -> io::Result<Self> {
        Self::bind_with_recorder(port, source, None).await
    }

    /// [`WebServer::bind`], with a recorder the UI may drive.
    ///
    /// Separate rather than a wider `bind` because `bind` has call sites with
    /// nothing to record — the tests and the `ui_preview` example — and none
    /// of them should grow a `None`.
    ///
    /// # Errors
    ///
    /// Whatever binding the loopback listener failed with, or if the bound
    /// address is somehow not loopback.
    pub async fn bind_with_recorder(
        port: u16,
        source: Arc<dyn MeetingSource>,
        recorder: Option<Arc<dyn crate::recorder::RecorderControl>>,
    ) -> io::Result<Self> {
        Self::bind_with_controls(port, source, recorder, None).await
    }

    /// [`WebServer::bind_with_recorder`], with the GitHub export control as
    /// well (issue #63).
    ///
    /// # Errors
    ///
    /// Whatever binding the loopback listener failed with, or if the bound
    /// address is somehow not loopback.
    pub async fn bind_with_controls(
        port: u16,
        source: Arc<dyn MeetingSource>,
        recorder: Option<Arc<dyn crate::recorder::RecorderControl>>,
        github: Option<Arc<dyn crate::github::GithubExport>>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
        let addr = listener.local_addr()?;
        if !addr.ip().is_loopback() {
            return Err(io::Error::other(format!(
                "refusing to serve: bound {addr}, which is not loopback"
            )));
        }
        let state = AppState::with_controls(
            IngressPolicy::for_loopback_port(addr.port()),
            source,
            recorder,
            github,
        );
        Ok(Self {
            listener,
            addr,
            state,
        })
    }

    /// The address that was actually bound, including the ephemeral port.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The state the handlers share — the secret, the token tables and the
    /// delta hub the pipeline publishes into.
    #[must_use]
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Serve until the process ends.
    ///
    /// # Errors
    ///
    /// Whatever `axum::serve` failed with.
    pub async fn serve(self) -> io::Result<()> {
        let app = router(self.state);
        // `into_make_service_with_connect_info` is what puts the peer address
        // in the extensions, which is what makes ING-01's tripwire more than a
        // comment.
        axum::serve(
            self.listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    }
}

/// The routed application, with [`guard`] wrapped around all of it.
///
/// # Why this is not `routes(state).layer(guard)`
///
/// `Router::layer` applies the layer to each *endpoint*, which puts it
/// **inside** the method router. That is late enough to matter: `axum`
/// attaches the `Allow` header to a method-not-allowed response after the
/// endpoint has returned, so a guard applied that way sees a bare 404 go out
/// and `allow: GET,HEAD` arrive at the client anyway — telling an
/// unauthenticated caller that `/api/meetings` exists (ING-09). Wrapping the
/// finished router in the layer instead makes the guard the outermost thing in
/// the stack, which is also what it should be for its own sake: it then runs
/// before routing, before every extractor, and before any rejection those
/// could produce.
pub fn router(state: AppState) -> Router {
    use tower::Layer as _;

    let guarded =
        axum::middleware::from_fn_with_state(state.clone(), guard).layer(routes(state.clone()));
    Router::new().fallback_service(guarded)
}

/// The routes, before the guard is wrapped around them.
///
/// Private, and it stays that way: a public constructor for an unguarded
/// router is a public constructor for a mistake. It is a separate function
/// only so that [`router`] reads as "these routes, guarded" in one line rather
/// than as a builder chain with a `.layer` buried in the middle of it.
fn routes(state: AppState) -> Router {
    Router::new()
        .route("/", get(assets::index))
        // axum 0.8 spells path parameters `{id}`; the URL is the `/:id` of the
        // spec either way.
        .route("/assets/{*path}", get(assets::asset))
        .route("/api/meetings", get(api::list_meetings))
        .route("/api/meetings/{id}", get(api::get_meeting))
        .route("/api/search", get(api::search))
        .route("/api/ws-ticket", post(api::ws_ticket))
        .route("/api/handoff", post(api::handoff))
        .route("/api/launch-url", post(api::launch_url))
        .route("/api/stream", get(stream::stream))
        .route("/api/recording/status", get(api::recording_status))
        .route("/api/recording/start", post(api::recording_start))
        .route("/api/recording/stop", post(api::recording_stop))
        .route(
            "/api/settings/github",
            get(api::github_settings).post(api::github_set_settings),
        )
        .route("/api/settings/github/repos", get(api::github_repos))
        .route("/api/meetings/{id}/github-push", post(api::github_push))
        // Both of these are ING-09. The fallback is the same bare 404 the
        // guard returns, so "wrong token" and "no such path" are one response.
        // The method fallback replaces axum's `405 Method Not Allowed` with an
        // `Allow` header — which would confirm that a path exists to a caller
        // who has no token.
        .fallback(bare_404)
        .method_not_allowed_fallback(bare_404)
        .with_state(state)
}

async fn bare_404() -> axum::response::Response {
    not_found()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;

    #[tokio::test]
    async fn it_binds_loopback_on_an_ephemeral_port() {
        let server = WebServer::bind(0, Arc::new(MemorySource::new()))
            .await
            .unwrap();
        let addr = server.addr();
        assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST, "ING-01");
        assert!(addr.port() != 0, "an ephemeral bind must report its port");
        // The allowlist is derived from the port that was actually bound, not
        // from the one that was asked for — the two differ every time here.
        assert_eq!(
            server.state().policy().authority(),
            format!("127.0.0.1:{}", addr.port())
        );
        assert_eq!(
            server.state().policy().origin(),
            format!("http://127.0.0.1:{}", addr.port())
        );
    }

    /// §5.5: `EADDRINUSE` on the loopback bind *is* the single-instance mutex,
    /// so it has to surface as an error rather than as a silent second bind.
    #[tokio::test]
    async fn a_second_daemon_on_the_same_port_fails_to_bind() {
        let first = WebServer::bind(0, Arc::new(MemorySource::new()))
            .await
            .unwrap();
        let port = first.addr().port();
        let second = WebServer::bind(port, Arc::new(MemorySource::new())).await;
        assert!(second.is_err(), "the second bind must fail");
    }
}
