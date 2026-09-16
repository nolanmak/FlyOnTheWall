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
//!
//! # Never a public repository by accident
//!
//! A commit cannot be taken back the way a local file can be deleted: it stays
//! in the repository's history, and a public repository serves that history
//! to anyone. What a push sends is the whole meeting: the transcript, the
//! summary and brief companions, and the title in the commit message and the
//! file name. So every write reads the repository's visibility from the same
//! `gh api repos/{repo}` answer the preflight already fetches, and refuses
//! with [`GithubError::RepoIsPublic`] unless GitHub says, unambiguously, that
//! it is private, or the stored target carries the user's acknowledgement,
//! [`GithubSettings::allow_public_repo`]. It is asked on every write rather
//! than once at save time because a repository can be made public after it
//! was configured. The repo picker leaves public repositories out, so a
//! misclick there cannot pick one.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use fotw_store::{Db, StoreError};
use fotw_web::{
    GithubError, GithubExport, GithubMode, GithubReceipt, GithubSettings, GithubSyncState,
    GithubSyncStatus,
};

use crate::audit::{AuditKind, AuditLog};
// The auto-push worker runs inside the daemon, whose stderr is discarded, so
// its diagnostics go to the daemon's log as well (#101).
use crate::{diag, note};

/// The `settings` key the target lives under, as `"retention"` does for §9.3.
pub const SETTINGS_KEY: &str = "github_export";

/// The `settings` key the per-meeting receipts live under: a JSON object of
/// meeting id → [`GithubReceipt`].
///
/// One row for every meeting ever pushed, rewritten whole on every push. The
/// cost model, the size at which it stops being free, and why both maps move
/// together or not at all are written up once on
/// [`crate::enrich::RECEIPTS_KEY`] (#89) — this entry is the older of the two
/// and set the precedent.
pub const RECEIPTS_KEY: &str = "github_export_receipts";

/// How long the auto pusher waits for enrichment before pushing anyway (#76).
///
/// The never-held-back-forever valve. Enrichment can die anywhere before it
/// writes its stamp — a keychain that will not open, a library that will not,
/// a daemon killed mid-pass — and a meeting waiting on a stamp nobody will
/// ever write is a meeting that is never exported.
///
/// Generous, because the worst case it has to outlast is a title call plus
/// Call A plus one Call B per chunk of a three-hour meeting; bounded, because
/// the whole point is that the wait ends.
const ENRICH_GRACE_MS: u64 = 30 * 60 * 1_000;

/// How long a meeting whose own push failed waits before the worker tries it
/// again (#112), by consecutive failure count; the last step repeats forever.
///
/// Before this, a failed push went into a set and was skipped until the daemon
/// restarted, and the per-meeting button was the only way back. Retrying once a
/// minute instead is how a laptop on hotel wifi meets a rate limiter, so the
/// wait escalates — and it is **bounded** at an hour, because a window that
/// doubled forever would park a meeting for a week and call it a retry.
const BACKOFF_MS: [u64; 3] = [5 * 60 * 1_000, 15 * 60 * 1_000, 60 * 60 * 1_000];

/// How many meetings one auto round may push (#112).
///
/// The whole-library switch can make thousands of meetings owed at once. Each
/// push is four `gh` calls and a commit on a shared branch, so a round that
/// tried to drain the lot would hold the write lock for an hour and introduce
/// the user's token to a secondary rate limit. Ten a minute drains six hundred
/// an hour, which finishes any real library overnight without the machine ever
/// looking busy — the same reasoning as `BACKFILL_PER_PASS` in `serve.rs`.
const MAX_PER_ROUND: usize = 10;

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
    /// Meetings whose push failed for a reason of their own. Not retried by
    /// the worker until the daemon restarts: retrying a hard failure once a
    /// minute is how a laptop on hotel wifi makes a rate limiter's
    /// acquaintance. Environment-wide failures — no gh, no login, no repo, a
    /// repo refused as public — never land here, so fixing the environment
    /// drains the backlog on the next poll. A manual push always tries afresh.
    failed_auto: Mutex<HashSet<String>>,
    /// Meetings with a push in progress right now. The Db lock is released
    /// for the whole gh sequence, so without this the worker and the UI
    /// button could push one meeting concurrently: both probe 404, both PUT
    /// without a sha, and the loser gets a spurious 422 for a transcript
    /// that in fact landed.
    in_flight: Mutex<HashSet<String>>,
    // GitHub updates a shared branch even when two files differ.
    writes: Mutex<()>,
}

impl std::fmt::Debug for GithubExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the root: it names the directory the meetings are in.
        f.write_str("GithubExporter(<redacted>)")
    }
}

/// Versions of the companion files at the configured destination.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ArtifactVersion {
    summary: Option<String>,
    document: Option<i64>,
    repo: String,
    branch: String,
}
const ARTIFACT_RECEIPTS_KEY: &str = "github_artifact_receipts";

impl ArtifactVersion {
    fn from_doc(doc: &fotw_store::export::MeetingDoc, settings: &GithubSettings) -> Self {
        Self {
            summary: doc.current_summary().map(|s| s.id.clone()),
            document: doc.documents.iter().map(|d| d.version).max(),
            repo: settings.repo.clone(),
            branch: settings.branch.clone(),
        }
    }
    fn has_files(&self) -> bool {
        self.summary.is_some() || self.document.is_some()
    }
}
fn read_artifact_receipts(db: &Db) -> HashMap<String, ArtifactVersion> {
    db.get_setting(ARTIFACT_RECEIPTS_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
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
            in_flight: Mutex::new(HashSet::new()),
            writes: Mutex::new(()),
        }
    }

    /// [`GithubExporter::new`] with the retry schedule named, in milliseconds.
    ///
    /// For tests, and honest about it: the real schedule is [`BACKOFF_MS`],
    /// whose first step is five minutes, and a test that had to wait five
    /// minutes to prove that a retry happens is a test nobody runs.
    #[must_use]
    pub fn with_retry_schedule(
        db: Db,
        sessions_root: PathBuf,
        runner: Arc<dyn GhRunner>,
        retry_after_ms: Vec<u64>,
    ) -> Self {
        // The schedule is honoured once the retry behaviour lands; until then
        // this is `new` with the seam in place.
        drop(retry_after_ms);
        Self::new(db, sessions_root, runner)
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
    ///
    /// **`ready` alone does not make a meeting owed** — see [`export_ready`].
    /// `persist.rs` sets that state before enrichment runs, and this worker is
    /// an independent thread polling for it every sixty seconds.
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
            let artifacts = read_artifact_receipts(&db);
            // Read once per round, beside the push receipts and for the same
            // reason: it is one settings row answering for every meeting.
            let enriched = crate::enrich::read_receipts(&db);
            let now = u64::try_from(fotw_store::now_ms()).unwrap_or(0);
            let skip = self
                .failed_auto
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Paged, newest first, stopping at the first meeting older than
            // the stamp: one page of 200 would silently strand an owed
            // meeting the moment a busy library outgrew it.
            let mut owed = Vec::new();
            let mut offset = 0;
            'pages: loop {
                let page = db.meetings().list(200, offset).unwrap_or_default();
                let full = page.len() == 200;
                for m in page {
                    if u64::try_from(m.started_at_ms).unwrap_or(0) < since {
                        break 'pages;
                    }
                    if m.state != "ready"
                        || skip.contains(&m.id)
                        || !export_ready(enriched.get(&m.id), m.updated_at, now)
                    {
                        continue;
                    }
                    let changed = receipts.contains_key(&m.id)
                        && db.export_meeting(&m.id).ok().is_some_and(|doc| {
                            let version = ArtifactVersion::from_doc(&doc, &settings);
                            version.has_files() && artifacts.get(&m.id) != Some(&version)
                        });
                    if !receipts.contains_key(&m.id) || changed {
                        owed.push(m.id);
                    }
                }
                if !full {
                    break;
                }
                offset += 200;
            }
            owed
        };

        let mut pushed = 0;
        for id in candidates {
            match GithubExport::push(self, &id) {
                Ok(receipt) => {
                    // The id and the repo, never the path: the path carries a
                    // slug of the meeting title, and titles are on §10's
                    // never-log list.
                    note!("  pushed     : {} -> {}", id, receipt.repo);
                    pushed += 1;
                }
                // The environment's fault, not this meeting's. Nothing is
                // parked, and the round ends: the same broken gh would answer
                // identically for every remaining meeting, once a minute.
                Err(e) if stalls_every_push(&e) => {
                    diag!("  ! GitHub pushes are stalled: {e}");
                    break;
                }
                Err(e) => {
                    diag!("  ! could not push meeting {id} to GitHub: {e}");
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

/// Whether a finished meeting has stopped changing enough to export (#76).
///
/// The gate is on **full enrichment**, not on the title looking human: the
/// exported Markdown embeds the current summary (`export.rs`), so a push
/// landing between the title and the summary would permanently ship a
/// summary-less file under a path the receipt then pins.
///
/// Three ways through, in the order they occur:
///
/// * a finished stamp — the ordinary path, seconds to minutes after persist;
/// * a pass that started long enough ago that it is not coming back. Anchored
///   at the start of enrichment rather than at `updated_at`, because a long
///   meeting spends a title call plus a Call B per chunk inside the window and
///   one measured from persist can expire mid-run — re-creating the exact race
///   the stamp closes;
/// * no stamp at all on a meeting nothing has touched in a while, which is
///   every meeting of a library recorded before any of this existed. That arm
///   is what stops the gate stranding an archive, and it also covers the
///   passes that die before the started stamp — a keychain or library that
///   would not open (`enrich.rs`'s wrapper).
fn export_ready(receipt: Option<&crate::enrich::EnrichReceipt>, updated_at: i64, now: u64) -> bool {
    let long_enough_ago = |then: u64| now.saturating_sub(then) > ENRICH_GRACE_MS;
    match receipt {
        Some(r) if r.finished_at_ms > 0 => true,
        // A stamp with no clocks at all reads as an ancient one rather than as
        // a pass in flight: the map is parsed tolerantly, and a field that did
        // not decode must not be able to hold a meeting back forever.
        Some(r) => long_enough_ago(r.started_at_ms),
        None => long_enough_ago(u64::try_from(updated_at).unwrap_or(0)),
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

    fn repos(&self) -> Result<Vec<String>, GithubError> {
        // One call, no preflight: the call's own failure modes already say
        // "no gh" and "no login", which is everything the picker needs to
        // know. Pushable repos only — offering a repo the token cannot write
        // to sets the user up for a push that fails later — and one page of
        // the 100 most recently pushed: a picker wants the repos someone
        // actually uses, and the field still accepts anything typed.
        //
        // Public repositories are left out, by the same rule
        // `confirmed_private` applies at push time: `private` must be true
        // and `visibility`, when present, must not say public. The picker is
        // where a misclick happens, and a misclick onto a public repository
        // in auto mode publishes every meeting after it. The trait's
        // `Vec<String>` has no room for a "public" label, so leaving them out
        // is the safe reading of it. A public repository typed by hand is
        // still refused by the preflight unless acknowledged.
        let args: Vec<String> = [
            "api",
            "user/repos?per_page=100&sort=pushed",
            "--jq",
            r#"[.[] | select(.permissions.push and .private == true and .visibility != "public") | .full_name]"#,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let out = self
            .runner
            .run(&args, None)
            .map_err(|_| GithubError::GhMissing)?;
        if out.status != 0 {
            return Err(classify(&out));
        }
        serde_json::from_str(&out.stdout).map_err(|_| {
            GithubError::Failed("gh answered something that is not a repo list".to_owned())
        })
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
        let (settings, markdown, path, title, started_at_ms, existing, companions, version) = {
            let mut db = self.lock_db();
            let settings = read_settings(&db);
            if !settings.enabled {
                return Err(GithubError::Disabled);
            }
            // The store's own error text can quote the row it choked on, and
            // this string reaches the UI and the daemon log — the same
            // reasoning that keeps api.rs's server_error() a bare 500.
            let meeting = db.meetings().get(meeting_id).map_err(|e| match e {
                StoreError::NotFound { .. } => GithubError::NoSuchMeeting,
                _ => GithubError::Failed("the library refused to read the meeting".to_owned()),
            })?;
            let doc = db.export_meeting(meeting_id).map_err(|_| {
                GithubError::Failed("the library refused to export the meeting".to_owned())
            })?;
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
            let version = ArtifactVersion::from_doc(&doc, &settings);
            let stem = path.strip_suffix(".md").unwrap_or(&path);
            let mut companions = Vec::new();
            if let Some(summary) = doc.current_summary() {
                companions.push((format!("{stem}.summary.md"), summary.body_md.clone()));
            }
            if let Some(row) = doc.documents.iter().max_by_key(|d| d.version) {
                let draft: fotw_web::documents::SharingDocument =
                    serde_json::from_str(&row.document_json).map_err(|_| {
                        GithubError::Failed("the saved meeting document could not be read".into())
                    })?;
                // Only the latest saved brief, never private review notes or revision history.
                companions.push((format!("{stem}.document.md"), draft.markdown));
            }
            // Claimed before the Db lock is released: from here to the
            // receipt write the meeting belongs to this call, and a second
            // push — the worker and the button racing — answers immediately
            // instead of double-committing.
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !in_flight.insert(meeting_id.to_owned()) {
                return Err(GithubError::Failed(
                    "a push for this meeting is already running".to_owned(),
                ));
            }
            drop(in_flight);
            let started = u64::try_from(meeting.started_at_ms).unwrap_or(0);
            let mut markdown = doc.to_markdown();
            if !companions.is_empty() {
                markdown.push_str("\n## Meeting documents\n\n");
                for (companion_path, _) in &companions {
                    let label = if companion_path.ends_with(".summary.md") {
                        "Summary"
                    } else {
                        "Saved document brief"
                    };
                    markdown.push_str(&format!("- [{label}]({})\n", basename(companion_path)));
                }
            }
            (
                settings,
                markdown,
                path,
                meeting.title,
                started,
                existing,
                companions,
                version,
            )
        };
        // Everything below must release the claim on every exit.
        let result = self.push_claimed(
            meeting_id,
            &settings,
            &markdown,
            &path,
            &title,
            started_at_ms,
            existing,
            &companions,
            &version,
        );
        self.in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(meeting_id);
        if result.is_ok() {
            self.failed_auto
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(meeting_id);
        }
        result
    }

    fn sync_status(&self, meeting_id: &str) -> GithubSyncStatus {
        // Answered from the receipts, the companion versions and the retry
        // table once #112's states land; until then it says nothing.
        let _ = meeting_id;
        GithubSyncStatus::off()
    }

    fn sync_bundle(&self) -> Result<(), GithubError> {
        let (settings, receipts) = {
            let db = self.lock_db();
            (read_settings(&db), read_receipts(&db))
        };
        if !settings.enabled {
            return Err(GithubError::Disabled);
        }
        // Only what was pushed to the *currently* configured repo belongs in
        // this bundle — a receipt from an old repo names a file that is not
        // here.
        let mut mine: Vec<GithubReceipt> = receipts
            .into_values()
            .filter(|r| r.repo == settings.repo)
            .collect();
        if mine.is_empty() {
            return Ok(());
        }
        // Newest first, deterministically: a tie on start time falls back to
        // the path so the listing does not reshuffle between runs.
        mine.sort_by(|a, b| {
            b.started_at_ms
                .cmp(&a.started_at_ms)
                .then_with(|| a.path.cmp(&b.path))
        });

        let entries: Vec<crate::okf::BundleEntry> = mine
            .iter()
            .map(|r| crate::okf::BundleEntry {
                filename: basename(&r.path).to_owned(),
                title: r.title.clone(),
                started_at_ms: r.started_at_ms,
                logged_at_ms: r.pushed_at_ms,
            })
            .collect();

        let _write = self
            .writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.preflight(&settings)?;
        let prefix = &settings.path_prefix;
        self.put_file(
            &settings,
            &format!("{prefix}index.md"),
            &crate::okf::render_index(&entries),
            "OKF index",
        )?;
        self.put_file(
            &settings,
            &format!("{prefix}log.md"),
            &crate::okf::render_log(&entries),
            "OKF change log",
        )?;
        Ok(())
    }
}

/// The file name inside the bundle directory — everything after the last `/`.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl GithubExporter {
    /// Run `gh`, mapping "could not even start it" to [`GithubError::GhMissing`].
    fn run_gh(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<GhOutput, GithubError> {
        let owned: Vec<String> = args.iter().map(ToString::to_string).collect();
        self.runner
            .run(&owned, stdin)
            .map_err(|_| GithubError::GhMissing)
    }

    /// The preflight questions every write shares: is anyone logged in, does
    /// the repo exist for them, and may a meeting land in it. Kept separate so
    /// a bundle sync and a meeting push ask them the same way.
    ///
    /// The third is the module docs' "Never a public repository by accident":
    /// unless `settings.allow_public_repo`, a repository GitHub does not
    /// confirm is private is refused with [`GithubError::RepoIsPublic`] before
    /// anything is written. `settings` is the caller's one snapshot of the row,
    /// so the acknowledgement checked is the one stored beside the repository
    /// being written to, not one saved a moment later for a different one.
    fn preflight(&self, settings: &GithubSettings) -> Result<(), GithubError> {
        let auth = self.run_gh(&["auth", "status", "--hostname", "github.com"], None)?;
        if auth.status != 0 {
            return Err(GithubError::NotAuthenticated);
        }
        let repo_url = format!("repos/{}", settings.repo);
        let repo = self.run_gh(&["api", &repo_url], None)?;
        if repo.status != 0 {
            if mentions_http(&repo.stderr, 404) {
                return Err(GithubError::RepoNotFound);
            }
            return Err(classify(&repo));
        }
        if !settings.allow_public_repo && !confirmed_private(&repo.stdout) {
            return Err(GithubError::RepoIsPublic);
        }
        Ok(())
    }

    /// Commit one file — create or update — and return the commit sha.
    ///
    /// `subject` is prefixed with `Add`/`Update` from whether the file already
    /// exists, so the commit log reads naturally for every file the bundle
    /// carries. Assumes [`GithubExporter::preflight`] already passed.
    fn put_file(
        &self,
        settings: &GithubSettings,
        path: &str,
        content: &str,
        subject: &str,
    ) -> Result<String, GithubError> {
        for attempt in 0..3 {
            match self.put_file_attempt(settings, path, content, subject) {
                Err(GithubError::Failed(message))
                    if attempt < 2 && mentions_http(&message, 409) => {}
                result => return result,
            }
        }
        unreachable!("the final attempt always returns")
    }

    fn put_file_attempt(
        &self,
        settings: &GithubSettings,
        path: &str,
        content: &str,
        subject: &str,
    ) -> Result<String, GithubError> {
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
        let probe = self.run_gh(&["api", &probe_url, "--jq", ".sha"], None)?;
        let sha = if probe.status == 0 {
            Some(probe.stdout.trim().to_owned()).filter(|s| !s.is_empty())
        } else if mentions_http(&probe.stderr, 404) {
            None
        } else {
            return Err(classify(&probe));
        };

        let mut body = serde_json::json!({
            "message": format!("{} {subject}", if sha.is_some() { "Update" } else { "Add" }),
            "content": B64.encode(content.as_bytes()),
        });
        if !settings.branch.is_empty() {
            body["branch"] = settings.branch.clone().into();
        }
        if let Some(sha) = &sha {
            body["sha"] = sha.clone().into();
        }

        let put_url = format!("repos/{}/contents/{}", settings.repo, path);
        let put = self.run_gh(
            &["api", "-X", "PUT", &put_url, "--input", "-"],
            Some(body.to_string().as_bytes()),
        )?;
        if put.status != 0 {
            return Err(classify(&put));
        }
        Ok(serde_json::from_str::<serde_json::Value>(&put.stdout)
            .ok()
            .and_then(|v| v["commit"]["sha"].as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".to_owned()))
    }

    /// The gh sequence and the bookkeeping, with the in-flight claim held.
    #[allow(clippy::too_many_arguments)]
    fn push_claimed(
        &self,
        meeting_id: &str,
        settings: &GithubSettings,
        markdown: &str,
        path: &str,
        title: &str,
        started_at_ms: u64,
        existing: Option<GithubReceipt>,
        companions: &[(String, String)],
        version: &ArtifactVersion,
    ) -> Result<GithubReceipt, GithubError> {
        let _write = self
            .writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.preflight(settings)?;

        let display_title = if title.trim().is_empty() {
            "Untitled meeting"
        } else {
            title.trim()
        };
        let commit = self.put_file(
            settings,
            path,
            markdown,
            &format!("meeting transcript: {display_title}"),
        )?;

        for (companion_path, content) in companions {
            self.put_file(settings, companion_path, content, "meeting document")?;
        }

        let receipt = GithubReceipt {
            repo: settings.repo.clone(),
            path: path.to_owned(),
            commit,
            pushed_at_ms: u64::try_from(fotw_store::now_ms()).unwrap_or(0),
            title: title.to_owned(),
            started_at_ms,
        };

        // The content is on GitHub now; what remains is remembering that
        // truthfully. The receipt is what stops auto mode pushing twice and
        // what routes a re-push to the same file; the audit line is CON-08's
        // record that this provider was contacted. Neither failing changes
        // what already happened, so both are loud rather than fatal.
        {
            let mut db = self.lock_db();
            let mut artifacts = read_artifact_receipts(&db);
            artifacts.insert(meeting_id.to_owned(), version.clone());
            let artifact_json = serde_json::to_string(&artifacts)
                .map_err(|_| GithubError::Failed("could not encode document receipts".into()))?;
            db.put_setting(ARTIFACT_RECEIPTS_KEY, &artifact_json)
                .map_err(|_| {
                    GithubError::Failed(
                        "files pushed but document receipt could not be saved".into(),
                    )
                })?;
            let mut receipts = read_receipts(&db);
            receipts.insert(meeting_id.to_owned(), receipt.clone());
            match serde_json::to_string(&receipts) {
                Ok(json) => {
                    if let Err(e) = db.put_setting(RECEIPTS_KEY, &json) {
                        diag!("  ! pushed, but could not save the receipt: {e}");
                        // Without the receipt, the worker would see this
                        // meeting as owed again next minute and commit it
                        // again, forever. Parking it caps the damage at one
                        // push; a restart (or a manual push) tries afresh.
                        self.failed_auto
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(meeting_id.to_owned());
                    }
                }
                Err(e) => diag!("  ! pushed, but could not encode the receipt: {e}"),
            }
        }
        if let Err(e) = AuditLog::at(&self.root).record(AuditKind::TranscriptPushed {
            meeting: meeting_id.to_owned(),
            repo: receipt.repo.clone(),
            path: receipt.path.clone(),
            commit: receipt.commit.clone(),
        }) {
            diag!("  ! pushed, but could not write the audit log: {e}");
        }

        let _ = existing; // the old receipt is fully superseded
        Ok(receipt)
    }
}

/// Does `gh`'s stderr name this HTTP status?
fn mentions_http(stderr: &str, code: u16) -> bool {
    stderr.contains(&format!("HTTP {code}"))
}

/// Whether `gh api repos/{repo}` answered that the repository is private.
///
/// Only an unambiguous yes counts: `private` must be the JSON `true`, and
/// `visibility`, when the answer carries it, must not say `public`. A missing
/// field, the two fields disagreeing, or an answer that is not a JSON object
/// all read as public. A wrong refusal costs one push; a wrong yes publishes a
/// meeting into a history that keeps it.
fn confirmed_private(repo_json: &str) -> bool {
    let Ok(repo) = serde_json::from_str::<serde_json::Value>(repo_json) else {
        return false;
    };
    repo["private"] == true && repo["visibility"] != "public"
}

/// How long to wait before retrying a meeting that has now failed `failures`
/// times in a row (#112).
///
/// The last step of `schedule` repeats rather than growing, which is what makes
/// the backoff bounded. A `failures` of zero cannot happen — the count is
/// incremented before this is asked — and answers the first step rather than
/// zero, because "no wait at all" is the one answer that would turn a retry
/// into the once-a-minute hammering this exists to stop.
fn backoff_ms(schedule: &[u64], failures: u32) -> u64 {
    // Implemented in the commit that adds the retry behaviour.
    let _ = (schedule, failures);
    0
}

/// The log line for a meeting entering backoff (#112).
///
/// Its own function so that a test can assert what reaches `fotwd.log` without
/// running a daemon. **It takes no title, by construction**: §10 keeps meeting
/// titles out of the log and the audit journal, and the only free text here is
/// `gh`'s own first line about a repository, which [`classify`] has already
/// bounded.
fn entered_backoff_line(meeting_id: &str, wait_ms: u64, reason: &str) -> String {
    let _ = (meeting_id, wait_ms, reason);
    String::new()
}

/// The log line for a meeting leaving backoff, for [`entered_backoff_line`]'s
/// reasons and with the same no-title guarantee.
fn left_backoff_line(meeting_id: &str, failures: u32) -> String {
    let _ = (meeting_id, failures);
    String::new()
}

/// A failure that would repeat identically for every meeting in an auto round:
/// no gh, no login, no repository, export switched off, or a repository the
/// preflight refuses as public. None is one meeting's fault, so the round
/// stops, nothing is parked, and fixing the cause drains the backlog on the
/// next poll.
///
/// An exhaustive match on purpose, so a new [`GithubError`] variant has to be
/// sorted into one side or the other here.
fn stalls_every_push(e: &GithubError) -> bool {
    match e {
        GithubError::GhMissing
        | GithubError::NotAuthenticated
        | GithubError::RepoNotFound
        | GithubError::Disabled
        | GithubError::RepoIsPublic => true,
        GithubError::NoSuchMeeting | GithubError::Invalid(_) | GithubError::Failed(_) => false,
    }
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

/// `prefix/YYYY-MM-DD-title-slug-idfragment.md` — the prefix plus the shared
/// [`crate::okf::transcript_filename`], so the GitHub path and a local OKF
/// export name the same meeting the same file.
fn transcript_path(prefix: &str, started_at_ms: i64, title: &str, meeting_id: &str) -> String {
    format!(
        "{prefix}{}",
        crate::okf::transcript_filename(started_at_ms, title, meeting_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unambiguous_private_answer_counts_as_private() {
        assert!(confirmed_private(
            r#"{"default_branch":"main","private":true,"visibility":"private"}"#
        ));
        // `visibility` is checked only when present; `private` is required.
        assert!(confirmed_private(r#"{"private":true}"#));

        for public in [
            r#"{"private":false,"visibility":"public"}"#,
            r#"{"private":true,"visibility":"public"}"#,
            r#"{"visibility":"private"}"#,
            r#"{"private":"true"}"#,
            r#"{"default_branch":"main"}"#,
            "[]",
            "",
            "not json",
        ] {
            assert!(!confirmed_private(public), "{public:?} must read as public");
        }
    }

    /// A6. Bounded *and* escalating. Escalating, because retrying a hard
    /// failure once a minute is how a laptop meets a rate limiter; bounded,
    /// because a window that doubled forever would park a meeting for a week
    /// and still call itself a retry.
    #[test]
    fn the_retry_schedule_escalates_and_then_settles_at_an_hour() {
        assert_eq!(backoff_ms(&BACKOFF_MS, 1), 5 * 60 * 1_000);
        assert_eq!(backoff_ms(&BACKOFF_MS, 2), 15 * 60 * 1_000);
        assert_eq!(backoff_ms(&BACKOFF_MS, 3), 60 * 60 * 1_000);
        for failures in [4, 12, 500] {
            assert_eq!(
                backoff_ms(&BACKOFF_MS, failures),
                60 * 60 * 1_000,
                "the last step repeats rather than growing without bound"
            );
        }
        assert_eq!(
            backoff_ms(&BACKOFF_MS, 0),
            5 * 60 * 1_000,
            "a zero-th failure cannot happen, and must not read as no wait"
        );
        assert_eq!(
            backoff_ms(&[], 3),
            0,
            "an empty schedule retries on the next round rather than panicking"
        );
    }

    /// A6 and A9 together. The log says which meeting, for how long and why,
    /// and the meeting is named by id: neither of these functions takes a
    /// title, which is what keeps §10's never-log rule true by construction
    /// rather than by review.
    #[test]
    fn the_backoff_log_lines_name_the_meeting_by_id_and_carry_no_title() {
        let id = "01926f5a-0000-7000-8000-000000000001";
        let entered = entered_backoff_line(id, 15 * 60 * 1_000, "gh: Validation Failed (HTTP 422)");
        assert!(entered.contains(id), "which meeting: {entered}");
        assert!(entered.contains("15 min"), "how long it waits: {entered}");
        assert!(entered.contains("HTTP 422"), "and why: {entered}");

        let left = left_backoff_line(id, 3);
        assert!(left.contains(id), "which meeting: {left}");
        assert!(left.contains('3'), "how many attempts it took: {left}");
    }

    #[test]
    fn a_public_refusal_stalls_the_round_and_a_meeting_failure_does_not() {
        assert!(stalls_every_push(&GithubError::RepoIsPublic));
        assert!(stalls_every_push(&GithubError::NotAuthenticated));
        assert!(!stalls_every_push(&GithubError::Failed(
            "gh: Validation Failed (HTTP 422)".to_owned()
        )));
        assert!(!stalls_every_push(&GithubError::NoSuchMeeting));
    }
}
