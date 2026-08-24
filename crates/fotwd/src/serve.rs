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

use fotw_web::{DaemonState, GithubExport, StoreSource, WebServer, write_state_file};

use crate::github::{GithubExporter, SystemGh};
use crate::retention::{self, Schedule, SweepMode, Tick};

/// How often the sweeper thread wakes to ask whether it may run.
///
/// Much finer than the hourly cadence itself, because the *other* question it
/// asks — "is a recording in flight" — changes on a human timescale. A sweep
/// deferred by a meeting should start shortly after the meeting ends, not at
/// the top of the next hour.
const SWEEP_POLL: Duration = Duration::from_secs(60);

/// How often auto mode asks whether a finished meeting is waiting to be
/// pushed to GitHub (issue #63).
///
/// A poll rather than a hook on the finish path: the finish path is shared
/// with the CLI and must never wait on the network, and a poll also catches
/// the meeting that finished while the daemon was down. One minute is
/// invisible next to "the meeting just ended".
const GITHUB_POLL: Duration = Duration::from_secs(60);

/// Where the CLI looks for a running daemon.
#[must_use]
pub fn state_file_path(root: &Path) -> PathBuf {
    root.parent().unwrap_or(root).join("daemon.json")
}

/// The port `serve` binds when `--port` does not say otherwise.
///
/// Fixed, because the login is keyed by origin and the port is part of an
/// origin: a stable port is what makes `http://127.0.0.1:8737` a bookmark
/// that works and lets every tab share one login. A fixed port is guessable
/// by a page scanning localhost, but the port was never a security control —
/// ING-01 through ING-05 are, and a scanner that finds the port still meets
/// the bearer. `--port 0` keeps the old harder-to-find ephemeral behavior
/// for anyone who wants the trade back.
pub const DEFAULT_PORT: u16 = 8737;

/// The port `serve` should bind, taken from the command line.
///
/// Defaults to [`DEFAULT_PORT`]; `--port 0` asks for an ephemeral one.
///
/// # Errors
///
/// If `--port` is present but its value is missing, not a number, above
/// 65535, or privileged.
pub fn parse_port(args: &[String]) -> Result<u16, String> {
    let Some(at) = args.iter().position(|a| a == "--port") else {
        return Ok(DEFAULT_PORT);
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

/// What a bare, argument-less invocation should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareLaunch {
    /// Print usage: a person in a terminal asking what this does.
    Usage,
    /// Start (or re-enter) the daemon and open the dashboard.
    Serve,
}

/// Decide what `fotwd` with no arguments means.
///
/// Finder launches `CFBundleExecutable` with no arguments and no terminal.
/// Printing usage to a stdout nobody can see and exiting is indistinguishable
/// from "the app doesn't open" — which is exactly how it was reported. So a
/// bare launch is the doorway when there is no terminal to read usage in, and
/// usage when there is: the same stdin test the Recovery Key ceremony uses to
/// tell a human apart from LaunchServices.
#[must_use]
pub fn bare_launch(stdin_is_terminal: bool) -> BareLaunch {
    if stdin_is_terminal {
        BareLaunch::Usage
    } else {
        BareLaunch::Serve
    }
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

/// Ask an already-running daemon for a fresh one-time login URL.
///
/// The caller proves it may ask by presenting the bearer from the 0600 state
/// file — the same file the CLI has always read. Any failure — nothing
/// listening, a foreign service on a reused port, a stale token — is an
/// `Err`, and the caller starts a daemon of its own instead.
///
/// # Errors
///
/// The daemon did not answer 200 with a loopback `?t=` URL.
pub async fn fetch_fresh_launch_url(port: u16, token: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/launch-url"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status().as_u16() != 200 {
        return Err(format!("not our daemon: status {}", response.status()));
    }
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    let url = body["url"].as_str().unwrap_or_default();
    if url.starts_with(&format!("http://127.0.0.1:{port}/?t=")) {
        Ok(url.to_owned())
    } else {
        Err("the answer did not look like a launch URL".to_owned())
    }
}

/// If a daemon is already serving this library, hand back a fresh login URL
/// from it. `None` means nobody real answered and a fresh start is in order.
async fn reopen_running_ui(root: &Path) -> Option<String> {
    let state = fotw_web::read_state_file(&state_file_path(root)).ok()?;
    fetch_fresh_launch_url(state.port, &state.token).await.ok()
}

/// Run the loopback server until the process is stopped.
pub async fn serve(root: PathBuf, launch: Launch, port: u16) -> Result<(), String> {
    // Second click, not second daemon: if one is already serving, every tab
    // problem is solved by a fresh one-time link from it — no keychain read,
    // no bind, no new process. This is also what makes the app icon behave
    // like an app: launching it again brings the UI back.
    if let Some(url) = reopen_running_ui(&root).await {
        match launch {
            Launch::OpenBrowser => match std::process::Command::new("open").arg(&url).status() {
                Ok(s) if s.success() => {
                    println!("  already running — opened a new authorized tab");
                }
                _ => println!("  already running — run `fotwd serve --print-url` for a link"),
            },
            Launch::PrintUrl => {
                println!("  already running — open this once, it expires in 30s:");
                println!("{url}");
            }
            Launch::Nothing => println!("  already running"),
        }
        return Ok(());
    }

    let db = crate::open_library(&root)?;
    let source = Arc::new(StoreSource::new(db));

    // 0 — the default — means the OS picks. A fixed port is guessable by a
    // page scanning localhost, and the ephemeral port is one more thing an
    // attacker has to find even though it is not itself a security control;
    // `--port` trades that away for an origin the browser can remember. See
    // `parse_port`.
    // The live-transcript producer (#61). The hub, its flusher and the
    // WebSocket have existed since the UI shipped; this closure is the first
    // thing in production to feed them. Deltas carry the session id — persist
    // mints the library id only at the end, and the renderer does not read
    // the id anyway; the post-stop refresh shows the persisted meeting.
    //
    // Late-bound because the hub lives inside the state that `bind` creates,
    // and the recorder must exist before `bind` is called. Until the slot is
    // filled the tap drops segments — which is a window nothing can occupy: a
    // recording can only be started over HTTP, and HTTP is not served yet.
    let hub_slot: Arc<std::sync::OnceLock<Arc<fotw_web::DeltaHub>>> =
        Arc::new(std::sync::OnceLock::new());
    let hub_for_tap = Arc::clone(&hub_slot);
    // `Delta.idx` is documented as the index within *the* transcript, so the
    // counter resets when the session changes — a daemon-lifetime counter
    // would leak how many segments earlier meetings produced.
    // Session-keyed live state: the delta index, and a short window of
    // recent SYSTEM finals for the live half of the cross-leg dedupe. The
    // persist-time pass is authoritative; this one keeps speaker echo off
    // the screen while the meeting runs. A mic final that arrives BEFORE
    // its system counterpart (the observed skew direction) leaks here and
    // is cleaned at persist — the documented post-stop-refresh contract.
    struct LiveTap {
        session: String,
        next_idx: i64,
        /// `(end_ms, classed tokens)` of recent system finals.
        recent_system: std::collections::VecDeque<(u64, Vec<String>)>,
    }
    let tap_state = Arc::new(std::sync::Mutex::new(LiveTap {
        session: String::new(),
        next_idx: 0,
        recent_system: std::collections::VecDeque::new(),
    }));
    let on_segment = crate::session::SegmentTap::new(move |seg, kind| {
        let Some(hub) = hub_for_tap.get() else { return };
        match kind {
            crate::session::TapKind::Final => {
                let mut guard = tap_state.lock().unwrap_or_else(|e| e.into_inner());
                if guard.session != seg.session_id {
                    guard.session = seg.session_id.clone();
                    guard.next_idx = 0;
                    guard.recent_system.clear();
                }
                match seg.source {
                    fotw_stt::Source::System => {
                        guard
                            .recent_system
                            .push_back((seg.end_ms, crate::session::dedupe_tokens(&seg.text)));
                        // Bounded both ways: count and age.
                        while guard.recent_system.len() > 8 {
                            guard.recent_system.pop_front();
                        }
                        let horizon = seg.end_ms.saturating_sub(20_000);
                        while guard
                            .recent_system
                            .front()
                            .is_some_and(|(end, _)| *end < horizon)
                        {
                            guard.recent_system.pop_front();
                        }
                    }
                    fotw_stt::Source::Mic => {
                        if crate::session::echoes_recent(
                            &seg.text,
                            guard.recent_system.iter().map(|(_, t)| t.as_slice()),
                        ) {
                            // Suppressed: skip the publish AND the index — a
                            // hole in `Delta.idx` would break its documented
                            // "index within the transcript" meaning. The
                            // renderer's pending mic row would otherwise
                            // strand the last echo partial on screen, so an
                            // empty partial clears it.
                            hub.publish(fotw_web::Delta {
                                text: String::new(),
                                ..delta_partial(seg)
                            });
                            return;
                        }
                    }
                }
                let idx = guard.next_idx;
                guard.next_idx += 1;
                hub.publish(delta_from(idx, seg));
            }
            // Partials take no index — they are revisions, not rows. The
            // renderer keeps one in-progress line per channel and replaces
            // it on every revision; the final then lands as a real row.
            crate::session::TapKind::Partial => hub.publish(delta_partial(seg)),
        }
    });

    // The recorder is what turns the read-only library viewer into something
    // that can start a meeting. It needs the runtime handle because `start()`
    // is called from a blocking pool thread and has to spawn the session task
    // back onto the runtime.
    let recorder = Arc::new(crate::recording::DaemonRecorder::new(
        root.clone(),
        tokio::runtime::Handle::current(),
        on_segment,
    ));

    // The GitHub export target (issue #63). Its own library connection, as
    // the sweeper has one, so a push never holds the UI's mutex through a
    // subprocess. Non-fatal on purpose: a UI without the export section
    // beats a UI that will not open — and the second open failing right
    // after the first succeeded is a story worth printing.
    let github = match crate::open_library(&root) {
        Ok(db) => Some(Arc::new(GithubExporter::new(
            db,
            root.clone(),
            Arc::new(SystemGh),
        ))),
        Err(e) => {
            eprintln!("  ! GitHub export is not available: {e}");
            None
        }
    };
    if let Some(exporter) = &github {
        spawn_github_pusher(Arc::clone(exporter));
    }

    let server = WebServer::bind_with_controls(
        port,
        source,
        Some(Arc::clone(&recorder) as Arc<dyn fotw_web::RecorderControl>),
        github.map(|g| g as Arc<dyn GithubExport>),
    )
    .await
    .map_err(|e| format!("could not bind 127.0.0.1: {e}"))?;

    let addr = server.addr();
    let state = server.state().clone();

    // Deltas are broadcast at 10 Hz rather than per word: a two-hour meeting
    // is ~20k words, and one message each would swamp the socket and the DOM.
    let hub = state.hub();
    let _flusher = hub.spawn_flusher();

    // The live-transcript tap can publish from here on.
    let _ = hub_slot.set(Arc::clone(hub));

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

    // macOS attributes a system-audio grant to the responsible process, which
    // for a daemon started from a shell is the terminal. Saying so here is the
    // difference between a user seeing a warning and a user getting a silent
    // recording they only discover afterwards.
    if !crate::recording::DaemonRecorder::launched_as_app() {
        println!("  ! Start will record through this terminal's audio grant, not");
        println!("    FlyOnTheWall's. For a real meeting, launch the bundle:");
        println!("      open -a FlyOnTheWall.app --args serve --port <n>");
    }

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

/// Start the auto-push worker on its own thread.
///
/// The same shape as the sweeper below, for the same reasons: everything it
/// does is blocking — SQLite, then a subprocess — and it is a long-lived
/// loop, not a unit of work. When manual mode or a disabled target makes
/// `auto_push_pending` a no-op, the loop is one settings read a minute.
fn spawn_github_pusher(exporter: Arc<GithubExporter>) {
    if let Err(e) = std::thread::Builder::new()
        .name("fotw-github".into())
        .spawn(move || {
            loop {
                exporter.auto_push_pending();
                std::thread::sleep(GITHUB_POLL);
            }
        })
    {
        // Worth a line, not a refusal: manual pushes still work.
        eprintln!("  ! automatic GitHub pushes are not running: {e}");
    }
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

/// One finalized segment, as the wire sees it.
///
/// `meeting_id` is the **session** id: a live recording has no library id —
/// persist mints one only when the meeting ends — and the UI's renderer
/// appends deltas without reading the id at all. Only finals reach the
/// collector that feeds this, so `is_final` is unconditionally true.
#[must_use]
pub fn delta_from(idx: i64, seg: &fotw_stt::TranscriptSegment) -> fotw_web::Delta {
    fotw_web::Delta {
        meeting_id: seg.session_id.clone(),
        idx,
        start_ms: i64::try_from(seg.start_ms).unwrap_or(i64::MAX),
        end_ms: i64::try_from(seg.end_ms).unwrap_or(i64::MAX),
        // §7.5's "me vs them", kept on the wire: the channel is what the UI
        // styles by, and losing it here would turn it back into a
        // diarisation problem.
        channel: match seg.source {
            fotw_stt::Source::Mic => "mic".to_owned(),
            fotw_stt::Source::System => "system".to_owned(),
        },
        text: seg.text.clone(),
        is_final: true,
    }
}

/// A still-revising utterance, as the wire sees it.
///
/// `idx` is `-1` and `is_final` false: a partial is a *revision*, not a row —
/// the next one with the same channel replaces it, and only the final gets a
/// real index. Publishing these is what makes the live view move while the
/// speaker is mid-sentence instead of at utterance boundaries.
#[must_use]
pub fn delta_partial(seg: &fotw_stt::TranscriptSegment) -> fotw_web::Delta {
    fotw_web::Delta {
        idx: -1,
        is_final: false,
        ..delta_from(-1, seg)
    }
}

/// Print the launch URL instead of opening a browser.
///
/// Separate from the default path because printing it puts a live credential
/// into terminal scrollback; the user asks for that explicitly.
pub fn print_launch_url(state: &fotw_web::AppState) {
    println!("{}", state.launch_url());
}
