//! The daemon's record of what it is doing — issue #101.
//!
//! Every diagnostic this daemon produces goes to `eprintln!`, and the stderr
//! of a LaunchServices-launched `.app` is discarded by macOS — verified
//! against `log show`, which carries framework subsystems and not one
//! application line. So on 2026-08-25, asked "is summarization working?", the
//! only way to answer was to kill the running daemon and relaunch it in a
//! terminal. Three wrong conclusions were drawn from the outside first — that
//! the backfill task had died, that `set_enrich_report` was never called, and
//! that the API did not serve `enrich_status`. All three were false. The
//! system was working correctly the whole time.
//!
//! This is the file that answers the question without killing anything.
//!
//! # Shape, and what it is copied from
//!
//! [`crate::audit::AuditLog`] is the precedent — the daemon already writes a
//! durable record next to the library — and this sits beside it, under the
//! same data root, with the same care: created deliberately at `0600`,
//! append-only, one record per line, `sync_data` before the call returns so a
//! crash cannot lose the line that explains it.
//!
//! It is **not** the audit log and must never be confused with one. That file
//! is CON-08's consent record and is never rewritten or discarded; this one is
//! diagnostics and rolls at [`DEFAULT_CAP_BYTES`], because the daemon runs for
//! weeks and an unbounded file gets deleted by somebody at exactly the moment
//! they most wanted to read it.
//!
//! # §10's never-log rule is a constraint on this module, not a note about it
//!
//! Transcript text, note text, meeting titles and attendee names may not reach
//! a file that persists. An ephemeral stderr can carry a title; this cannot.
//! The two call sites that handle transcript-derived text therefore do not
//! write it — [`meeting_titled`] writes a length, [`meeting_problems`] writes
//! a count and points at `meetings.enrich_detail`, which the API already
//! serves and the dashboard already renders (#74). Both are pure functions so
//! that the rule is pinned by a test rather than by a reviewer's attention.
//!
//! # Why there is no logging framework here
//!
//! Because the issue's non-goal says so, and because it would be the wrong
//! shape: the daemon's own lifecycle is the surface, not every crate. The call
//! sites keep their `eprintln!` wording and gain [`diag!`] / [`note!`], which
//! say the same thing to the terminal *and* to this file. Stderr still matters
//! — `fotwd record` and `fotwd serve` run from a terminal, where it works
//! fine — so nothing is redirected away from it.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// How large the live file may get before it rolls.
///
/// Two megabytes is roughly a fortnight of a working daemon at the observed
/// rate — an hourly backfill pass, an hourly sweep, a push round a minute —
/// and one generation is kept behind it, so the worst case on disk is twice
/// this beside a library measured in gigabytes.
pub const DEFAULT_CAP_BYTES: u64 = 2 * 1024 * 1024;

/// The longest a single record may be.
///
/// A cap rather than a courtesy: a provider body or a child process's stderr
/// arrives here as one string, and a megabyte of it would evict the history
/// around it — which is the part that explains what happened.
const MAX_LINE_CHARS: usize = 1_000;

/// The daemon's diagnostics file.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    cap_bytes: u64,
    /// Held across the size check and the write, so two background threads
    /// cannot both decide to roll. The write itself needs no lock — `O_APPEND`
    /// makes each one atomic against the file offset — but "look, then
    /// rename" is not one operation.
    gate: Mutex<()>,
}

impl Journal {
    /// The journal beside a sessions root, at [`DEFAULT_CAP_BYTES`].
    #[must_use]
    pub fn at(sessions_root: &Path) -> Self {
        Self::with_cap(sessions_root, DEFAULT_CAP_BYTES)
    }

    /// [`Journal::at`] with the cap named.
    ///
    /// The seam that makes rolling testable: filling two megabytes to prove a
    /// rename happens is a test nobody runs twice.
    #[must_use]
    pub fn with_cap(sessions_root: &Path, cap_bytes: u64) -> Self {
        let dir = sessions_root.parent().unwrap_or(sessions_root);
        Self {
            path: dir.join("fotwd.log"),
            cap_bytes,
            gate: Mutex::new(()),
        }
    }

    /// Where it lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The one generation kept behind the live file.
    #[must_use]
    pub fn rolled_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    }

    /// Append one line, stamped now.
    ///
    /// # Errors
    ///
    /// If the file cannot be created, written or flushed.
    pub fn record(&self, message: &str) -> io::Result<()> {
        self.record_at(now_ms(), message)
    }

    /// Append one line with an explicit timestamp.
    ///
    /// # Errors
    ///
    /// If the file cannot be created, written or flushed.
    pub fn record_at(&self, at_unix_ms: u64, message: &str) -> io::Result<()> {
        let line = format!("{}  {}\n", stamp(at_unix_ms), one_line(message));

        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.roll_if_full(line.len() as u64)?;

        let mut file = private_append(&self.path)?;
        // One `write_all` of a complete line, into a descriptor opened
        // `O_APPEND`: a reader tailing this file never sees half a record from
        // one thread wrapped around half a record from another.
        file.write_all(line.as_bytes())?;
        // Flushed like the audit log's, and for a sharper reason: the line
        // most worth having is the one written immediately before whatever
        // killed the process.
        file.sync_data()
    }

    /// The most recent `n` lines, oldest first.
    ///
    /// A journal that was never written is empty, not an error — the daemon
    /// may not have said anything yet.
    ///
    /// # Errors
    ///
    /// If the file exists but cannot be read.
    pub fn tail(&self, n: usize) -> io::Result<Vec<String>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let all: Vec<&str> = text.lines().collect();
        Ok(all[all.len().saturating_sub(n)..]
            .iter()
            .map(|l| (*l).to_owned())
            .collect())
    }

    /// Move the live file aside when the next line would overflow the cap.
    ///
    /// Exactly one generation. A numbered pile would need a policy for pruning
    /// it, and the question this file answers — "what has the daemon done
    /// recently" — is never asked of the fourth-oldest generation.
    fn roll_if_full(&self, incoming: u64) -> io::Result<()> {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return Ok(());
        };
        // `> 0` guards the pathological case: one record longer than the whole
        // cap would otherwise roll on every write and leave the live file
        // permanently empty, which is worse than one oversized file.
        if meta.len() > 0 && meta.len() + incoming > self.cap_bytes {
            std::fs::rename(&self.path, self.rolled_path())?;
        }
        Ok(())
    }
}

/// Open the journal for appending, `0600` from the moment it exists.
///
/// Set at creation, not after: a file that spends a millisecond at `0644` has
/// been readable by every other account on the machine for a millisecond, and
/// this one carries meeting ids and the daemon's own failures.
#[cfg(unix)]
fn private_append(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_append(path: &Path) -> io::Result<std::fs::File> {
    // The same debt `fotw_web::state_file` records: Windows ACLs are a
    // different API, the daemon is macOS-first, and the compile-time split is
    // where that is written down rather than a silently permissive fallback.
    OpenOptions::new().create(true).append(true).open(path)
}

/// Tighten a journal that predates this code, or a looser umask.
///
/// The mode above protects a file this process created. It does nothing for
/// one that was already there — and unlike the state file, this one is never
/// rewritten, so there is no later moment that would fix it.
#[cfg(unix)]
fn tighten(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten(_path: &Path) -> io::Result<()> {
    Ok(())
}

// -------------------------------------------------------- the process's own

/// The journal this process writes to, once [`install`] has been called.
static INSTALLED: OnceLock<Journal> = OnceLock::new();

/// Open the journal for this process and prove it is writable.
///
/// Called once, early in `fotwd serve`, before anything that might have
/// something to say. The probe line is the install itself: a log that reports
/// its own failure only on the hundredth line has already lost the first
/// ninety-nine.
///
/// # Errors
///
/// If the file cannot be created or written. The caller says so on stderr and
/// carries on — a daemon that refuses to record meetings because it could not
/// open its diagnostics file has its priorities exactly backwards.
pub fn install(sessions_root: &Path) -> io::Result<&'static Path> {
    let journal = INSTALLED.get_or_init(|| Journal::at(sessions_root));
    tighten(journal.path())?;
    journal.record(&format!(
        "daemon   : log opened (rolls at {} KiB, one generation kept)",
        DEFAULT_CAP_BYTES / 1024
    ))?;
    Ok(journal.path())
}

/// Where this process's journal is, if it has one.
#[must_use]
pub fn installed_path() -> Option<&'static Path> {
    INSTALLED.get().map(Journal::path)
}

/// Write one line to this process's journal, if it has one.
///
/// A no-op before [`install`], which is what makes it safe to call from
/// library code the CLI also runs: `fotwd summarize` has a terminal and no
/// journal, and neither fact should change what the code around it looks like.
///
/// Failures are dropped on purpose. There is nowhere left to report a failure
/// to report things, and taking a recording down over it would be absurd.
pub fn record(message: &str) {
    if let Some(journal) = INSTALLED.get() {
        let _ = journal.record(message);
    }
}

/// Say something to the terminal's **stderr** and to the journal.
///
/// The existing `eprintln!` wording travels unchanged; the leading indent the
/// terminal lines carry is trimmed on the way into the file, where the
/// timestamp occupies that column instead.
#[macro_export]
macro_rules! diag {
    ($($arg:tt)*) => {{
        let line = ::std::format!($($arg)*);
        ::std::eprintln!("{line}");
        $crate::journal::record(&line);
    }};
}

/// Say something to the terminal's **stdout** and to the journal.
///
/// [`diag!`]'s counterpart for the lines a person watching `fotwd serve` reads
/// as progress rather than as trouble.
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => {{
        let line = ::std::format!($($arg)*);
        ::std::println!("{line}");
        $crate::journal::record(&line);
    }};
}

// ------------------------------------------------------- §10's never-log rule

/// What the journal is told when enrichment names a meeting.
///
/// `recording.rs` prints `meeting titled: {title}` to stderr, which is fine on
/// a stream nobody keeps and is a §10 violation the moment it persists: a
/// title is written *from the transcript* and is on the never-log list beside
/// the transcript itself. The id is what makes the line actionable — it is
/// what `fotwd summarize <id>` takes — and the length is what makes it
/// diagnostic, because a zero-length title is a bug this file has to be able
/// to show. The stderr line keeps the title; only the durable copy loses it.
#[must_use]
pub fn meeting_titled(meeting_id: &str, title: &str) -> String {
    format!(
        "enrich   : meeting {meeting_id} titled, {} chars",
        title.chars().count()
    )
}

/// What the journal is told when enrichment had problems.
///
/// Never the problems themselves. On the CLI arm a `problems` entry is a child
/// process's stderr, produced over a prompt built from the transcript, and on
/// the API arm it can carry a provider body or a model's own output — all of
/// it §10 material, none of it safe to persist without reading every future
/// engine's mind.
///
/// It costs nothing to leave out, because the detail is already stored where
/// it belongs: `meetings.enrich_status` and `enrich_detail`, written by every
/// pass, served by the API and rendered by the dashboard (#74). This line's
/// job is to say that there *is* something to look at and where — which is the
/// half that was missing.
#[must_use]
pub fn meeting_problems(meeting_id: &str, problems: &[String]) -> String {
    if problems.is_empty() {
        format!("enrich   : meeting {meeting_id} reported no problems")
    } else {
        format!(
            "enrich   : meeting {meeting_id} reported {} problem(s) — the text is in \
             enrich_detail, which the dashboard renders",
            problems.len()
        )
    }
}

// ------------------------------------------------------ the daemon's own life

/// What the journal is told when `serve` begins.
///
/// The pid is what `kill` takes, and what tells two launches apart in a log
/// that outlives both. The sessions root is which library this daemon opened,
/// which is the first question on a machine that has more than one.
///
/// §10 leaves both alone: a sessions root is a directory a person named or the
/// default under Application Support, not text written from a transcript.
#[must_use]
pub fn serve_starting(pid: u32, sessions_root: &Path) -> String {
    format!(
        "daemon   : serve starting — pid {pid}, sessions {}",
        sessions_root.display()
    )
}

/// What the journal is told when `serve` returns — issue #102.
///
/// #101 recorded the attempt and not the outcome. On the first rebuild after
/// it landed, a keychain ACL that had never been approved (#53) produced two
/// fatal startups and four log lines that said only that they had begun — the
/// reason went to the stderr of a LaunchServices-launched `.app`, which is the
/// thing macOS discards and the whole premise of the journal.
///
/// The clean arm matters as much as the fatal one, for a reason that is easy
/// to miss: a `Ctrl-C` or a `kill` never reaches this seam at all. So a
/// journal whose last line is `listening` was ended from outside, one whose
/// last line is this one stopped serving on its own, and one whose last line
/// names a reason never got up. Three different endings, told apart only
/// because the middle one says something.
///
/// # Why the reason is written out in full, under §10
///
/// Every string that can arrive here is a startup failure, and there are four
/// producers: the library open — a platform error and a keychain item
/// *account*, `db:masterkey`, never material, and
/// [`fotw_secrets::SecretsError`] is built so that no variant of it can carry
/// credential bytes; the bind — an `io::Error` and a port; the state-file
/// write — a path and an `io::Error`; and the server itself. None of them can
/// reach a transcript, a title or an attendee.
///
/// That is what separates this from [`meeting_problems`], which is handed the
/// engine's own output over a prompt built from the transcript and may not
/// persist a word of it. The test is what a string can carry, not which file
/// it happens to be written in.
#[must_use]
pub fn serve_exit(outcome: &Result<(), String>) -> String {
    match outcome {
        Ok(()) => "daemon   : serve exited — the server stopped without an error".to_owned(),
        Err(why) => format!("daemon   : ! serve exited: {why}"),
    }
}

// ------------------------------------------------------------------ the pulse

/// A line worth writing when it changes, and once in a while regardless.
///
/// The shape the polling loops need. `spawn_github_pusher` wakes once a
/// minute: writing every wake is 1440 lines a day that bury the ones worth
/// reading, and writing none of them is precisely the bug #101 is about — "it
/// had nothing to do" and "the thread is dead" have to look different from
/// outside. So an unchanged answer is repeated on a slow clock, and a changed
/// one immediately.
#[derive(Debug)]
pub struct Pulse {
    every_ms: u64,
    last: Option<(String, u64)>,
}

impl Pulse {
    /// A pulse that repeats an unchanged line once an hour.
    #[must_use]
    pub const fn hourly() -> Self {
        Self::every(3_600_000)
    }

    /// [`Pulse::hourly`] on a named interval.
    #[must_use]
    pub const fn every(every_ms: u64) -> Self {
        Self {
            every_ms,
            last: None,
        }
    }

    /// Whether this line should be written now, remembering that it was.
    ///
    /// The clock is only re-anchored when the answer is yes, so a line
    /// suppressed at minute 30 does not push its own hourly repeat to minute
    /// 90.
    pub fn due(&mut self, now_ms: u64, line: &str) -> bool {
        let due = match &self.last {
            Some((said, at)) => said != line || now_ms.saturating_sub(*at) >= self.every_ms,
            None => true,
        };
        if due {
            self.last = Some((line.to_owned(), now_ms));
        }
        due
    }
}

// ----------------------------------------------------------------- formatting

/// `YYYY-MM-DDTHH:MM:SSZ` from epoch milliseconds.
///
/// UTC, and it says so. The workspace carries no timezone database at all —
/// see `enrich::dated_fallback_title` — so UTC is the only clock this can name
/// honestly, and a log read weeks later needs an absolute one.
#[must_use]
pub fn stamp(at_unix_ms: u64) -> String {
    let ms = i64::try_from(at_unix_ms).unwrap_or(i64::MAX);
    let (y, mo, d) = crate::okf::ymd_utc(ms);
    let second_of_day = ms.div_euclid(1_000).rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60
    )
}

/// One record, one line, bounded.
///
/// A sweep report and a provider body both arrive here carrying newlines, and
/// a record that becomes three lines is three events to anyone counting them.
/// Runs of control characters collapse to a single space so the result still
/// reads as a sentence.
fn one_line(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for c in message.trim().chars() {
        if c.is_control() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        if out.chars().count() >= MAX_LINE_CHARS {
            out.push_str(" …");
            break;
        }
    }
    out.trim_end().to_owned()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
