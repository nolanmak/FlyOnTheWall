//! `fotwd serve` — the loopback UI.
//!
//! Binds an ephemeral port on `127.0.0.1`, writes a 0600 state file so the
//! CLI can find the daemon, and opens the user's own browser at a one-time
//! handoff URL.
//!
//! # Why the browser rather than an embedded webview
//!
//! Devtools, zoom, extensions and a shareable second window come free, and
//! WebKit stays out of our process and off the notarisation surface. The cost
//! is that the page is reachable by every other page the user visits, which
//! is why `fotw-web` carries twelve ingress controls rather than a login form.
//!
//! # The launch URL is a one-time token, not the session secret
//!
//! `open(1)` puts its argument in the process argument vector, readable by any
//! same-user process, and the URL also lands in the browser's synced history.
//! So the URL carries a handoff token worth exactly one redemption inside
//! thirty seconds; the bearer token is handed to the page in the response body
//! and never appears in a URL.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fotw_web::{DaemonState, StoreSource, WebServer, write_state_file};

/// Where the CLI looks for a running daemon.
#[must_use]
pub fn state_file_path(root: &Path) -> PathBuf {
    root.parent().unwrap_or(root).join("daemon.json")
}

/// How the page gets its handoff token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// Hand the URL straight to the default browser. The token never appears
    /// anywhere the user can read it, which is the point.
    OpenBrowser,
    /// Print it, for a headless box, a remote session, or a browser that is
    /// not the default. Opt-in only: this puts a live credential into terminal
    /// scrollback, where it outlives the session that made it.
    PrintUrl,
    /// Start the server and say nothing.
    Nothing,
}

/// Run the loopback server until the process is stopped.
pub async fn serve(root: PathBuf, launch: Launch) -> Result<(), String> {
    let db = crate::open_library(&root)?;
    let source = Arc::new(StoreSource::new(db));

    // Port 0: the OS picks. A fixed port would be trivially guessable by a
    // page scanning localhost, and the ephemeral port is one more thing an
    // attacker has to find even though it is not itself a security control.
    let server = WebServer::bind(0, source)
        .await
        .map_err(|e| format!("could not bind 127.0.0.1: {e}"))?;

    let addr = server.addr();
    let state = server.state().clone();

    // Deltas are broadcast at 10 Hz rather than per word: a two-hour meeting
    // is ~20k words, and one message each would swamp the socket and the DOM.
    let hub = state.hub();
    let _flusher = hub.spawn_flusher();

    let daemon = DaemonState {
        port: addr.port(),
        token: state.policy().secret().expose_hex(),
    };
    let path = state_file_path(&root);
    write_state_file(&path, &daemon).map_err(|e| format!("writing {}: {e}", path.display()))?;

    println!("  listening  : http://{addr}");
    println!("  state file : {} (0600)", path.display());

    match launch {
        Launch::OpenBrowser => {
            let url = state.launch_url();
            // Deliberately not printed: it carries the handoff token, and a
            // terminal scrollback is a place secrets survive.
            match std::process::Command::new("open").arg(&url).status() {
                Ok(s) if s.success() => println!("  opened your browser"),
                _ => println!("  could not open a browser — run `fotwd serve --print-url`"),
            }
        }
        Launch::PrintUrl => {
            println!("  open this once, it expires in 30s:");
            print_launch_url(&state);
        }
        Launch::Nothing => {}
    }

    println!();
    println!("  Ctrl-C to stop.");
    server.serve().await.map_err(|e| format!("server: {e}"))
}

/// Print the launch URL instead of opening a browser.
///
/// Separate from the default path because printing it puts a live credential
/// into terminal scrollback; the user asks for that explicitly.
pub fn print_launch_url(state: &fotw_web::AppState) {
    println!("{}", state.launch_url());
}
