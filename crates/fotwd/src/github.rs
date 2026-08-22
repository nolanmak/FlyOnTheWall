//! The GitHub export target (issue #63): push a finished meeting's Markdown
//! export into a repository the user configured in the dashboard.
//!
//! # Why `gh`, not an HTTP client and a stored token
//!
//! The commit is made by the GitHub CLI the user already authenticated —
//! `gh auth login` keeps the credential in the OS keyring, scopes it, and
//! refreshes it. This process never sees a token, so KEY-01's "no key
//! anywhere but the keychain" holds trivially: there is no key. It is the
//! same shape as EXP-06's Notion decision (a user-owned credential, a direct
//! API), one step further out. Composio and its kind were rejected outright:
//! a hosted integration platform is a vendor relay for meeting content, and
//! §2 exists to forbid exactly that.
//!
//! The network call happens inside the `gh` child process rather than the
//! daemon's allowlisted HTTP client, so the CON-08 obligation is discharged
//! here instead: every push writes a `transcript_pushed` line to the local
//! audit log — the fact of the egress, never the content.
//!
//! # One commit per meeting, Contents API, no clone
//!
//! `gh api -X PUT repos/{owner}/{repo}/contents/{path}` commits one file.
//! The request body travels on stdin (`--input -`): a two-hour transcript
//! base64s past ARG_MAX, and an argv is visible to every process of this
//! user anyway. The path is minted once — `prefix/date-slug-id.md` — and
//! remembered in a receipt, so a re-push after an edit updates the same file
//! instead of scattering copies, even if the meeting was retitled in between.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use fotw_store::{Db, StoreError};
use fotw_web::{GithubError, GithubExport, GithubMode, GithubReceipt, GithubSettings};

use crate::audit::{AuditKind, AuditLog};

/// The `settings` key the target lives under, as `"retention"` does for §9.3.
pub const SETTINGS_KEY: &str = "github_export";

/// The `settings` key the per-meeting receipts live under: a JSON object of
/// meeting id → [`GithubReceipt`].
pub const RECEIPTS_KEY: &str = "github_export_receipts";

/// What one `gh` invocation came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhOutput {
    /// The exit status; 0 is success.
    pub status: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr — where `gh` writes its `HTTP 404`-style failures.
    pub stderr: String,
}

/// How `gh` gets run.
///
/// A trait for the same reason the recorder takes a [`TapOpener`]
/// (crate::recording::TapOpener): the real thing spawns a process that talks
/// to the network, and every test would rather script it.
pub trait GhRunner: Send + Sync {
    /// Run `gh` with `args`, feeding `stdin` if given.
    ///
    /// # Errors
    ///
    /// Only when the process could not be spawned at all — no binary. A `gh`
    /// that ran and failed is an `Ok` with a non-zero status.
    fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Result<GhOutput, String>;
}

/// The real `gh`, found on `PATH` or in the usual install locations.
///
/// Resolved on every call rather than once, so installing `gh` fixes the
/// "install gh" error without restarting the daemon.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGh;

impl GhRunner for SystemGh {
    fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Result<GhOutput, String> {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let program = resolve_gh().ok_or_else(|| "gh is not installed".to_owned())?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start gh: {e}"))?;

        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A write error here means gh died early; the exit status below
            // is the better story than this one.
            let _ = pipe.write_all(bytes);
        }

        let out = child
            .wait_with_output()
            .map_err(|e| format!("could not wait for gh: {e}"))?;
        Ok(GhOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// `PATH` first — the user's choice wins — then the places Homebrew and a
/// pkg installer put it, because a daemon launched by LaunchServices gets the
/// minimal `PATH` that misses all three.
fn resolve_gh() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("gh")).collect())
        .unwrap_or_default();
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/gh"),
        PathBuf::from("/usr/local/bin/gh"),
    ]);
    candidates.into_iter().find(|p| p.is_file())
}

/// Stores the target, pushes transcripts, remembers what it pushed.
///
/// Owns its own library connection (the sweeper precedent — §9.1's
/// `busy_timeout` exists for the occasional second writer), so the UI's
/// [`StoreSource`](fotw_web::StoreSource) mutex never waits on a subprocess.
pub struct GithubExporter {
    db: Mutex<Db>,
    /// The sessions root, which is where the audit log lives beside.
    root: PathBuf,
    runner: Arc<dyn GhRunner>,
    /// Meetings whose auto-push failed. Not retried until the daemon
    /// restarts: retrying a hard failure once a minute is how a laptop on
    /// hotel wifi makes a rate limiter's acquaintance. A manual push always
    /// tries afresh.
    failed_auto: Mutex<HashSet<String>>,
}

impl std::fmt::Debug for GithubExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the root: it names the directory the meetings are in.
        f.write_str("GithubExporter(<redacted>)")
    }
}

impl GithubExporter {
    /// An exporter over its own library connection.
    #[must_use]
    pub fn new(db: Db, sessions_root: PathBuf, runner: Arc<dyn GhRunner>) -> Self {
        Self {
            db: Mutex::new(db),
            root: sessions_root,
            runner,
            failed_auto: Mutex::new(HashSet::new()),
        }
    }

    fn lock_db(&self) -> MutexGuard<'_, Db> {
        self.db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Where a meeting's transcript already lives in the repo, if it was ever
    /// pushed.
    #[must_use]
    pub fn receipt_for(&self, meeting_id: &str) -> Option<GithubReceipt> {
        read_receipts(&self.lock_db()).remove(meeting_id)
    }

    /// Push every finished meeting auto mode owes and has not pushed yet.
    ///
    /// Returns how many landed. Failures are said out loud, remembered, and
    /// not retried until restart; a meeting from before the auto stamp is
    /// not owed at all — enabling auto must never export the archive.
    pub fn auto_push_pending(&self) -> usize {
        let settings = read_settings(&self.lock_db());
        if !settings.enabled || settings.mode != GithubMode::Auto {
            return 0;
        }
        let Some(since) = settings.auto_since_ms else {
            // Auto with no stamp would mean "everything, ever" — refuse.
            return 0;
        };

        let candidates: Vec<String> = {
            let mut db = self.lock_db();
            let receipts = read_receipts(&db);
            let skip = self
                .failed_auto
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            db.meetings()
                .list(200, 0)
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.state == "ready")
                .filter(|m| u64::try_from(m.started_at_ms).unwrap_or(0) >= since)
                .filter(|m| !receipts.contains_key(&m.id))
                .filter(|m| !skip.contains(&m.id))
                .map(|m| m.id)
                .collect()
        };

        let mut pushed = 0;
        for id in candidates {
            match GithubExport::push(self, &id) {
                Ok(receipt) => {
                    println!("  pushed     : {} -> {}/{}", id, receipt.repo, receipt.path);
                    pushed += 1;
                }
                Err(e) => {
                    eprintln!("  ! could not push meeting {id} to GitHub: {e}");
                    self.failed_auto
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(id);
                }
            }
        }
        pushed
    }
}

/// The settings in force, falling back to the defaults — a missing or
/// unparseable row is a fresh library, never an error. The same shape as
/// [`crate::retention::settings`], for the same reason.
fn read_settings(db: &Db) -> GithubSettings {
    db.get_setting(SETTINGS_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn read_receipts(db: &Db) -> HashMap<String, GithubReceipt> {
    db.get_setting(RECEIPTS_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

impl GithubExport for GithubExporter {
    fn settings(&self) -> GithubSettings {
        read_settings(&self.lock_db())
    }

    fn set_settings(&self, settings: GithubSettings) -> Result<GithubSettings, GithubError> {
        let mut db = self.lock_db();
        let previous = read_settings(&db);

        let mut next = settings;
        // The stamp is what keeps "switch auto on" meaning "meetings from
        // now on" rather than "my entire archive, tonight". It survives
        // re-saves and dies with auto itself.
        let auto_now = next.enabled && next.mode == GithubMode::Auto;
        let auto_before = previous.enabled && previous.mode == GithubMode::Auto;
        next.auto_since_ms = if auto_now {
            match previous.auto_since_ms {
                Some(stamp) if auto_before => Some(stamp),
                _ => Some(u64::try_from(fotw_store::now_ms()).unwrap_or(0)),
            }
        } else {
            None
        };

        let json = serde_json::to_string(&next)
            .map_err(|e| GithubError::Failed(format!("could not encode the settings: {e}")))?;
        db.put_setting(SETTINGS_KEY, &json)
            .map_err(|e| GithubError::Failed(format!("could not store the settings: {e}")))?;
        Ok(next)
    }

    fn push(&self, meeting_id: &str) -> Result<GithubReceipt, GithubError> {
        // Snapshot under the lock, then let it go: the gh calls below take
        // seconds, and the auto worker shares this exporter with the UI.
        let (settings, markdown, path, title, existing) = {
            let mut db = self.lock_db();
            let settings = read_settings(&db);
            if !settings.enabled {
                return Err(GithubError::Disabled);
            }
            let meeting = db.meetings().get(meeting_id).map_err(|e| match e {
                StoreError::NotFound { .. } => GithubError::NoSuchMeeting,
                other => GithubError::Failed(format!("could not read the meeting: {other}")),
            })?;
            let doc = db
                .export_meeting(meeting_id)
                .map_err(|e| GithubError::Failed(format!("could not export the meeting: {e}")))?;
            let existing = read_receipts(&db).remove(meeting_id);
            let path = existing.as_ref().map_or_else(
                || {
                    transcript_path(
                        &settings.path_prefix,
                        meeting.started_at_ms,
                        &meeting.title,
                        meeting_id,
                    )
                },
                |r| r.path.clone(),
            );
            (settings, doc.to_markdown(), path, meeting.title, existing)
        };

        let run = |args: &[&str], stdin: Option<&[u8]>| {
            let owned: Vec<String> = args.iter().map(ToString::to_string).collect();
            self.runner
                .run(&owned, stdin)
                .map_err(|_| GithubError::GhMissing)
        };

        // Three questions before the write, each with its own honest answer:
        // is there a gh, is anyone logged in, does the repo exist for them.
        let auth = run(&["auth", "status", "--hostname", "github.com"], None)?;
        if auth.status != 0 {
            return Err(GithubError::NotAuthenticated);
        }

        let repo_url = format!("repos/{}", settings.repo);
        let repo = run(&["api", &repo_url], None)?;
        if repo.status != 0 {
            if mentions_http(&repo.stderr, 404) {
                return Err(GithubError::RepoNotFound);
            }
            return Err(classify(&repo));
        }

        // Create or update? The Contents API wants the old blob's sha for an
        // update and refuses one for a create, so ask first.
        let probe_url = if settings.branch.is_empty() {
            format!("repos/{}/contents/{}", settings.repo, path)
        } else {
            format!(
                "repos/{}/contents/{}?ref={}",
                settings.repo, path, settings.branch
            )
        };
        let probe = run(&["api", &probe_url, "--jq", ".sha"], None)?;
        let sha = if probe.status == 0 {
            Some(probe.stdout.trim().to_owned()).filter(|s| !s.is_empty())
        } else if mentions_http(&probe.stderr, 404) {
            None
        } else {
            return Err(classify(&probe));
        };

        let display_title = if title.trim().is_empty() {
            "Untitled meeting"
        } else {
            title.trim()
        };
        let mut body = serde_json::json!({
            "message": format!(
                "{} meeting transcript: {display_title}",
                if sha.is_some() { "Update" } else { "Add" }
            ),
            "content": B64.encode(markdown.as_bytes()),
        });
        if !settings.branch.is_empty() {
            body["branch"] = settings.branch.clone().into();
        }
        if let Some(sha) = &sha {
            body["sha"] = sha.clone().into();
        }

        let put_url = format!("repos/{}/contents/{}", settings.repo, path);
        let put = run(
            &["api", "-X", "PUT", &put_url, "--input", "-"],
            Some(body.to_string().as_bytes()),
        )?;
        if put.status != 0 {
            return Err(classify(&put));
        }

        let commit = serde_json::from_str::<serde_json::Value>(&put.stdout)
            .ok()
            .and_then(|v| v["commit"]["sha"].as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());

        let receipt = GithubReceipt {
            repo: settings.repo.clone(),
            path,
            commit,
            pushed_at_ms: u64::try_from(fotw_store::now_ms()).unwrap_or(0),
        };

        // The content is on GitHub now; what remains is remembering that
        // truthfully. The receipt is what stops auto mode pushing twice and
        // what routes a re-push to the same file; the audit line is CON-08's
        // record that this provider was contacted. Neither failing changes
        // what already happened, so both are loud rather than fatal.
        {
            let mut db = self.lock_db();
            let mut receipts = read_receipts(&db);
            receipts.insert(meeting_id.to_owned(), receipt.clone());
            match serde_json::to_string(&receipts) {
                Ok(json) => {
                    if let Err(e) = db.put_setting(RECEIPTS_KEY, &json) {
                        eprintln!("  ! pushed, but could not save the receipt: {e}");
                    }
                }
                Err(e) => eprintln!("  ! pushed, but could not encode the receipt: {e}"),
            }
        }
        if let Err(e) = AuditLog::at(&self.root).record(AuditKind::TranscriptPushed {
            meeting: meeting_id.to_owned(),
            repo: receipt.repo.clone(),
            path: receipt.path.clone(),
            commit: receipt.commit.clone(),
        }) {
            eprintln!("  ! pushed, but could not write the audit log: {e}");
        }

        let _ = existing; // the old receipt is fully superseded
        Ok(receipt)
    }
}

/// Does `gh`'s stderr name this HTTP status?
fn mentions_http(stderr: &str, code: u16) -> bool {
    stderr.contains(&format!("HTTP {code}"))
}

/// The catch-all mapping for a `gh` invocation that failed for a reason the
/// call site did not already recognise.
fn classify(out: &GhOutput) -> GithubError {
    if mentions_http(&out.stderr, 401) || mentions_http(&out.stderr, 403) {
        return GithubError::NotAuthenticated;
    }
    // First line only, bounded: gh error text is safe to show (it is GitHub's
    // error message, never our request body), but nobody needs a page of it.
    let line = out.stderr.lines().next().unwrap_or("").trim();
    let mut short: String = line.chars().take(200).collect();
    if short.is_empty() {
        short = format!("gh exited with status {}", out.status);
    }
    GithubError::Failed(short)
}

/// `prefix/YYYY-MM-DD-title-slug-idfragment.md`.
///
/// Deterministic from fields that exist at push time, with the id fragment
/// carrying uniqueness — two "Standup" meetings on the same day must not
/// fight over one filename.
fn transcript_path(prefix: &str, started_at_ms: i64, title: &str, meeting_id: &str) -> String {
    let (y, m, d) = ymd_utc(started_at_ms);
    let id8: String = meeting_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    format!("{prefix}{y:04}-{m:02}-{d:02}-{}-{id8}.md", slug(title))
}

/// Lowercased, everything unsafe collapsed to `-`, bounded, never empty.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.chars().count() >= 48 {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "meeting".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Civil date from epoch milliseconds, UTC. Hinnant's `civil_from_days`,
/// which is exact over the whole proleptic Gregorian calendar — no leap-year
/// table to get wrong.
fn ymd_utc(epoch_ms: i64) -> (i64, u32, u32) {
    let days = epoch_ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 { y + 1 } else { y },
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_civil_date_math_is_right_where_it_matters() {
        assert_eq!(ymd_utc(0), (1970, 1, 1));
        assert_eq!(ymd_utc(1_755_734_400_000), (2025, 8, 21));
        // A leap day, and the millisecond before it.
        assert_eq!(ymd_utc(1_709_164_800_000), (2024, 2, 29));
        assert_eq!(ymd_utc(1_709_164_800_000 - 1), (2024, 2, 28));
        // Before the epoch: div_euclid, not integer division toward zero.
        assert_eq!(ymd_utc(-1), (1969, 12, 31));
    }

    #[test]
    fn a_slug_survives_hostile_titles() {
        assert_eq!(slug("Weekly Standup"), "weekly-standup");
        assert_eq!(slug("Q3 // planning: part 2!"), "q3-planning-part-2");
        assert_eq!(slug(""), "meeting");
        assert_eq!(slug("---"), "meeting");
        assert_eq!(
            slug("Café müde"),
            "café-müde",
            "unicode letters are letters"
        );
        assert!(slug(&"x".repeat(500)).chars().count() <= 48);
    }

    #[test]
    fn the_path_is_prefix_date_slug_and_id_fragment() {
        let p = transcript_path(
            "meetings/",
            1_755_734_400_000,
            "Weekly Standup",
            "01890c2a-ffff-7000-8000-000000000000",
        );
        assert_eq!(p, "meetings/2025-08-21-weekly-standup-01890c2a.md");
        // An empty prefix lands in the repository root, without a leading /.
        assert_eq!(
            transcript_path("", 0, "", "abc"),
            "1970-01-01-meeting-abc.md"
        );
    }
}
