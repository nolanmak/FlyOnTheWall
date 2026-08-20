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
use std::time::Duration;

use fotw_web::{DaemonState, StoreSource, WebServer, write_state_file};

use crate::retention::{self, Schedule, SweepMode, Tick};

/// How often the sweeper thread wakes to ask whether it may run.
///
/// Much finer than the hourly cadence itself, because the *other* question it
/// asks — "is a recording in flight" — changes on a human timescale. A sweep
/// deferred by a meeting should start shortly after the meeting ends, not at
/// the top of the next hour.
const SWEEP_POLL: Duration = Duration::from_secs(60);

/// Where the CLI looks for a running daemon.
#[must_use]
pub fn state_file_path(root: &Path) -> PathBuf {
    root.parent().unwrap_or(root).join("daemon.json")
}

/// The port `serve` should bind, taken from the command line.
///
/// Defaults to 0 — the OS picks — because a fixed port is guessable by a page
/// scanning localhost. That is not itself a security control (ING-01 through
/// ING-05 are), but it is one more thing an attacker has to find, so trading
/// it away stays an explicit choice.
///
/// The reason to offer the trade at all: the redeemed bearer lives in
/// `sessionStorage` (ING-08), which is keyed by origin, and the port is part
/// of an origin. An ephemeral port therefore throws the credential away on
/// every restart, and the user meets a 30-second handoff window instead of a
/// bookmark that works.
///
/// # Errors
///
/// If `--port` is present but its value is missing, not a number, above
/// 65535, or privileged.
pub fn parse_port(args: &[String]) -> Result<u16, String> {
    let Some(at) = args.iter().position(|a| a == "--port") else {
        return Ok(0);
    };

    // A following `--flag` is treated as absent rather than as the value:
    // `--port --print-url` must not bind whatever that parsed to.
    let raw = args
        .get(at + 1)
        .filter(|v| !v.starts_with("--"))
        .ok_or_else(|| "--port needs a number, as in `--port 8765`".to_owned())?;

    // Parsed as u32 rather than u16 so that 65536 reports the boundary
    // instead of the same "not a number" a typo produces.
    let port: u32 = raw
        .parse()
        .map_err(|_| format!("--port: `{raw}` is not a port number"))?;

    if port > u32::from(u16::MAX) {
        return Err(format!("--port: {port} is above the maximum of 65535"));
    }
    if port != 0 && port < 1024 {
        return Err(format!(
            "--port: {port} is privileged — ports below 1024 need root, and the \
             error the OS returns for that is a bare `Permission denied` that \
             never mentions the port. Pick one above 1024."
        ));
    }

    u16::try_from(port).map_err(|e| format!("--port: {e}"))
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
pub async fn serve(root: PathBuf, launch: Launch, port: u16) -> Result<(), String> {
    let db = crate::open_library(&root)?;
    let source = Arc::new(StoreSource::new(db));

    // 0 — the default — means the OS picks. A fixed port is guessable by a
    // page scanning localhost, and the ephemeral port is one more thing an
    // attacker has to find even though it is not itself a security control;
    // `--port` trades that away for an origin the browser can remember. See
    // `parse_port`.
    let server = WebServer::bind(port, source)
        .await
        .map_err(|e| format!("could not bind 127.0.0.1: {e}"))?;

    let addr = server.addr();
    let state = server.state().clone();

    // Deltas are broadcast at 10 Hz rather than per word: a two-hour meeting
    // is ~20k words, and one message each would swamp the socket and the DOM.
    let hub = state.hub();
    let _flusher = hub.spawn_flusher();

    // §9.5's sweeper, and issue #41's "on app start and hourly". Started
    // before the URL is printed so a daemon that is killed a second later
    // still finished whatever promotion the last run interrupted.
    //
    // Non-fatal on purpose: a sweeper that cannot start is a disk that fills
    // slowly, which is a far smaller problem than a UI that will not open at
    // all. It is said out loud rather than swallowed.
    if let Err(e) = spawn_sweeper(&root) {
        eprintln!("  ! retention is not running: {e}");
        eprintln!("    Audio will accumulate. `fotwd retention` shows what is on disk.");
    }

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

/// Start the retention sweeper on its own thread.
///
/// # Why a thread and not a tokio task
///
/// Everything the sweeper does is blocking: SQLite queries, `stat`, `unlink`.
/// On a runtime worker that would stall the UI's request handling for the
/// length of a sweep. `spawn_blocking` would work too, but the sweeper is a
/// long-lived loop rather than a unit of work, and parking a blocking-pool
/// slot forever is a worse shape than one dedicated thread.
///
/// # Why a second connection
///
/// The UI's [`Db`](fotw_store::Db) is sealed inside `StoreSource`'s mutex with
/// no accessor. SQLite in WAL mode serialises writers and `Db::open` sets
/// `busy_timeout = 5000` (§9.1), so a second writer that touches a handful of
/// rows once an hour is exactly the case that setting exists for.
fn spawn_sweeper(root: &Path) -> Result<(), String> {
    let sessions = root.to_path_buf();
    let data_root = sessions.parent().unwrap_or(&sessions).to_path_buf();
    let mut db = crate::open_library(root)?;

    std::thread::Builder::new()
        .name("fotw-sweeper".into())
        .spawn(move || {
            let mut schedule = Schedule::hourly();
            loop {
                let now = fotw_store::now_ms().max(0) as u64;
                // The veto, checked every time and never overridden by how
                // long it has been. Competing for disk I/O with a live capture
                // is how buffers get dropped.
                let recording = retention::recording_in_flight(&sessions, now);
                if schedule.poll(now, recording) == Tick::Run {
                    sweep_once(&mut db, &data_root);
                }
                std::thread::sleep(SWEEP_POLL);
            }
        })
        .map_err(|e| format!("could not start the retention sweeper: {e}"))?;
    Ok(())
}

/// One pass: finish interrupted promotions, then apply the retention policy.
///
/// Loud about everything irreversible and quiet about everything else. A sweep
/// that deletes nothing — the overwhelmingly common case — prints nothing, so
/// the lines that do appear are all deletions.
fn sweep_once(db: &mut fotw_store::Db, data_root: &Path) {
    for outcome in retention::resume_promotions(db, data_root) {
        match outcome {
            Ok(p) => println!("  archived   : {} ({} bytes)", p.rel_dir, p.bytes()),
            Err(e) => eprintln!("  ! could not archive a pending session: {e}"),
        }
    }

    let now = fotw_store::now_ms().max(0) as u64;
    // `Apply`, because the policy the user configured is the consent. Every
    // protection that makes that safe — `forever`, un-transcribed audio, the
    // transcripts themselves — lives in `plan_sweep` and is tested there.
    match retention::sweep(db, data_root, now, SweepMode::Apply) {
        Ok(report) => {
            if !report.plan.evictions.is_empty()
                || !report.plan.warnings.is_empty()
                || !report.errors.is_empty()
            {
                print!("{}", report.render());
            }
        }
        Err(e) => eprintln!("  ! retention sweep failed: {e}"),
    }
}

/// Print the launch URL instead of opening a browser.
///
/// Separate from the default path because printing it puts a live credential
/// into terminal scrollback; the user asks for that explicitly.
pub fn print_launch_url(state: &fotw_web::AppState) {
    println!("{}", state.launch_url());
}
