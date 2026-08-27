//! What a meeting gets the moment it finalizes — #67, #68, #76, #88.
//!
//! The jarvis shape: `fotwd serve` stops being a recorder with homework and
//! becomes the thing that hands you a titled, summarised meeting with its
//! action items extracted, seconds after you hit Stop.
//!
//! # Enrichment never fails the meeting
//!
//! Everything here is *derived*. The audio and the transcript are already on
//! disk before this module runs, so every failure inside it is a report, not
//! an error — the same posture as `stt_errors`, and for the same reason: a
//! failure nobody can see is a failure nobody fixes, but a failure that takes
//! the meeting down with it is worse.
//!
//! # Who else reads what this module writes
//!
//! Enrichment now leaves an [`EnrichReceipt`] per meeting, and the GitHub
//! exporter reads it. `persist.rs` marks a meeting `ready` *before* this
//! module runs, and `spawn_github_pusher` polls for exactly that state every
//! sixty seconds — so on 2026-08-25 a transcript reached a repository six
//! seconds before its title existed, under a path the push receipt then
//! pinned forever. The stamp is what the exporter waits on (#76).
//!
//! # What runs with no engine at all
//!
//! The fallback title. Nothing leaves the machine: there is nowhere for it to
//! go — and a transcriptless meeting (recorded with no provider) is left
//! entirely alone, because silence is a normal state, not a fault.
//!
//! # The meetings that predate the receipt
//!
//! The map is the only thing that separates a title this machine minted from a
//! title a person typed, so a library recorded before it existed has thirty-odd
//! meetings the guard reads as renames and will never improve. They are
//! recoverable exactly, without a heuristic: [`adopt_legacy_titles`] recomputes
//! [`fallback_title`] over each meeting's own segments and adopts the title
//! only when the bytes match, which is a proof of authorship rather than a
//! guess at one (#88).
//!
//! # Why the report is persisted and not just printed
//!
//! "No engine", "an engine that will not resolve here" and "an engine that
//! ran and failed" used to be one thing on screen: nothing. `problems` went
//! to `eprintln!`, and the daemon's stderr belongs to a LaunchServices `.app`
//! that discards it — verified against `log show`, which carries framework
//! subsystems and not one application line. So every pass now also writes
//! `meetings.enrich_status`/`enrich_detail`, which the API serves and the
//! dashboard renders where the blank space used to be (#74).
//!
//! The write is best-effort by the same rule as everything else here: a
//! report that could not be stored is one more problem, never an error.

use std::collections::HashMap;
use std::path::Path;

use fotw_secrets::KeyStore;
use fotw_store::Db;
use fotw_summarize::template::{TemplateSet, default_templates_dir};
use serde::{Deserialize, Serialize};

use crate::engine::{
    Engine, EngineResolution, fallback_title, resolve_engine, resolve_engine_detailed,
};
use crate::summarize::{SummarizeRunError, summarize_meeting_on, title_meeting_on};

/// The prefix the title minted at persist time carries. A title starting with
/// it is a placeholder nobody chose, and always replaceable.
///
/// Not the whole of the guard — see [`EnrichReceipt::minted_title`].
/// `fallback_title`'s answer is a sentence lifted from the transcript and
/// carries no prefix at all, so a prefix test alone classifies this machine's
/// own first-utterance title as a human rename and refuses to ever improve it.
const FALLBACK_PREFIX: &str = "Untitled recording";

/// The `settings` key the per-meeting enrichment stamps live under.
///
/// A settings row rather than a column, on [`crate::github`]'s push-receipt
/// precedent: bookkeeping *about* a meeting rather than a property of one, and
/// it needs no migration.
pub const RECEIPTS_KEY: &str = "enrich_receipts";

/// What enrichment left behind for one meeting: two clocks and a string, each
/// with exactly one reader.
///
/// Tolerant on the way in — a missing or unparseable row is a library that has
/// never enriched, never an error — the same shape as [`crate::github`]'s
/// receipts and [`crate::engine::SummarizeSettings`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnrichReceipt {
    /// When the last pass **began**, epoch milliseconds.
    ///
    /// The GitHub exporter's grace anchor (#76), measured from the start of
    /// enrichment rather than from persist time on purpose: a long meeting
    /// spends a title call plus Call A plus a Call B per chunk in here, and a
    /// window measured from persist can expire mid-run — which re-creates the
    /// export race this stamp exists to close.
    pub started_at_ms: u64,
    /// When the last pass **ended**, epoch milliseconds; `0` while one runs.
    ///
    /// Non-zero is what makes a meeting exportable. It means a pass ran to the
    /// end — with a summary, without one, or with nothing but a fallback title
    /// — never that the pass succeeded.
    pub finished_at_ms: u64,
    /// The title enrichment itself last wrote.
    ///
    /// What makes a machine title replaceable. A later pass may improve a title
    /// this machine minted; a rename typed by a human changes the string, stops
    /// matching, and is never touched again. There is no rename endpoint yet —
    /// this guard is what will make one safe (REQUIREMENTS.md:75).
    pub minted_title: String,
}

/// Every meeting's enrichment stamp.
///
/// Read once per round by [`crate::github::GithubExporter::auto_push_pending`],
/// beside the push receipts it already reads the same way.
#[must_use]
pub fn read_receipts(db: &Db) -> HashMap<String, EnrichReceipt> {
    db.get_setting(RECEIPTS_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// The name a meeting gets the moment it is persisted, before enrichment has
/// seen it — #67's transcript-less half.
///
/// Dated rather than stamped. `Untitled recording — 1787535722` is what a
/// speechless recording was actually called in the live library, and an epoch
/// second is not something a person reads. The clock is UTC and says so:
/// nothing here can turn an IANA name into an offset — `persist.rs` reads
/// `/etc/localtime` for the *name*, and there is no timezone database anywhere
/// in this dependency graph — and a wall-clock time silently hours out is worse
/// than one that names the clock it is on.
///
/// Still [`FALLBACK_PREFIX`]-prefixed, so the replaceability guard is
/// unaffected and a later pass with a transcript still improves it.
#[must_use]
pub fn dated_fallback_title(now_ms: i64) -> String {
    let (y, m, d) = crate::okf::ymd_utc(now_ms);
    let minute_of_day = now_ms.div_euclid(60_000).rem_euclid(1_440);
    format!(
        "{FALLBACK_PREFIX} — {y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        minute_of_day / 60,
        minute_of_day % 60
    )
}

/// What one enrichment pass did.
#[derive(Debug, Default)]
pub struct EnrichReport {
    /// The title now on the meeting, when this pass set one.
    pub title: Option<String>,
    /// The stored summary's version, when a summary was generated.
    pub summary_version: Option<i64>,
    /// Everything that went wrong, in the order it went wrong.
    pub problems: Vec<String>,
}

/// Enrich against an open library and credential store.
pub async fn enrich_meeting_with(
    db: &mut Db,
    store: &dyn KeyStore,
    meeting_id: &str,
) -> EnrichReport {
    let mut report = EnrichReport::default();

    // The started stamp, before anything else: it is the grace anchor the
    // GitHub exporter measures against (#76), and it has to be on disk before
    // the minutes of LLM calls below, not after them.
    let mut receipt = read_receipts(db).remove(meeting_id).unwrap_or_default();
    receipt.started_at_ms = u64::try_from(fotw_store::now_ms()).unwrap_or(0);
    receipt.finished_at_ms = 0;
    stamp(db, meeting_id, &receipt, &mut report);

    // No transcript is a normal state — recorded with no provider — and an
    // empty one gets the same treatment.
    let segments = crate::summarize::stored_segments(db, meeting_id).unwrap_or_default();

    // Whether this machine may name the meeting at all. Both halves matter:
    // the prefix catches the placeholder minted at persist time, and the
    // receipt catches a first-utterance title an earlier engineless pass wrote
    // — which carries no prefix and would otherwise be mistaken for a rename.
    let current = db
        .meetings()
        .get(meeting_id)
        .map(|m| m.title)
        .unwrap_or_default();
    let replaceable = current.trim().is_empty()
        || current.starts_with(FALLBACK_PREFIX)
        || current == receipt.minted_title;

    // The summary, when there is anything to summarise. Its markdown is
    // versioned and append-only (§8.9); the action items ride inside it,
    // extracted and evidence-validated by the pipeline's Call B.
    //
    // A transcriptless meeting is skipped *without* a report: "no engine" is
    // true of it and useless, and it would park a meeting that has nothing to
    // summarise in the backfill sweeper's queue for good.
    if !segments.is_empty() {
        let (status, detail) = match resolve_engine_detailed(store, db) {
            EngineResolution::NoneConfigured => {
                report.problems.push(NO_ENGINE.to_owned());
                ("no_engine", None)
            }
            EngineResolution::Unresolvable { configured } => {
                report.problems.push(unresolvable(&configured));
                ("engine_unresolvable", Some(configured))
            }
            // Resolved **once**, and carried down. Both calls below used to go
            // back and resolve for themselves, so one meeting cost three trips
            // through the keystore — and on the Anthropic arm three keychain
            // reads, through the store whose 5-second timeout exists because
            // those can block (#87). The answer cannot sensibly change inside
            // one pass, and where it does — a binary uninstalled between the
            // title call and Call A — the spawn says so by name, which is a
            // better report than the "no engine is configured" a second
            // resolve produced for a machine that plainly has one.
            EngineResolution::Engine(engine) => {
                // The title **first**, which is what makes #67's "within
                // seconds of finalising" true: it is one small call over the
                // head of the transcript and it answers in seconds, where the
                // summary below is minutes. The comment that used to sit at
                // the bottom of this function reserved the seam for a
                // summary-derived title; a separate call is better, because it
                // lands even when the templates are missing or Call B fails.
                if replaceable {
                    title_from_engine(
                        db,
                        &engine,
                        meeting_id,
                        &segments,
                        &mut receipt,
                        &mut report,
                    )
                    .await;
                }
                summarize(db, &engine, meeting_id, &mut report).await
            }
        };
        // Best-effort, per the module docs: a report that could not be stored
        // is one more problem, never an error.
        //
        // The status is the *summary's*, never the title's. A title call that
        // failed over a summary that landed is `ok` with a problem beside it,
        // for #75's reason: "Summary failed" printed over a summary the user
        // can read is a lie about the thing the column is named for. The title
        // failure is not lost — it is in `problems`, which reaches the report.
        if let Err(e) = db
            .meetings()
            .set_enrich_report(meeting_id, status, detail.as_deref())
        {
            report.problems.push(format!("enrich report: {e}"));
        }
    }

    // The local fallback, for everything the engine did not name: no engine at
    // all, an engine that would not resolve here, an engine that ran and
    // answered with something that is not a title.
    if report.title.is_none() {
        if replaceable {
            if let Some(fallback) = fallback_title(&segments) {
                mint(db, meeting_id, &fallback, &mut receipt, &mut report);
            }
        } else {
            report.title = Some(current);
        }
    }

    // The finished stamp, last and on every path — no engine, no transcript, a
    // failing engine alike. It is what tells the GitHub exporter this meeting
    // has stopped changing (#76). The paths that never reach here at all — a
    // keychain or library failure in [`enrich_meeting`] — are exactly what the
    // exporter's grace window is for.
    receipt.finished_at_ms = u64::try_from(fotw_store::now_ms()).unwrap_or(0);
    stamp(db, meeting_id, &receipt, &mut report);

    report
}

/// Ask the engine to name the meeting, and store the answer if it is a name.
///
/// Every failure is a `problems` line and nothing more: the caller falls
/// through to [`fallback_title`], so a refused title costs the meeting a good
/// name and never its summary.
async fn title_from_engine(
    db: &mut Db,
    engine: &Engine,
    meeting_id: &str,
    segments: &[fotw_stt::TranscriptSegment],
    receipt: &mut EnrichReceipt,
    report: &mut EnrichReport,
) {
    match title_meeting_on(engine, meeting_id, segments).await {
        Ok(named) => mint(db, meeting_id, &named, receipt, report),
        Err(e) => report.problems.push(format!("title: {e}")),
    }
}

/// Store a machine-minted title, and remember that this machine minted it.
///
/// The remembering is the point. Without it a first-utterance title is
/// indistinguishable from a rename the user typed, and could never be improved
/// by a later pass — the wrinkle that left every meeting in the live library
/// named after its opening sentence.
fn mint(
    db: &mut Db,
    meeting_id: &str,
    title: &str,
    receipt: &mut EnrichReceipt,
    report: &mut EnrichReport,
) {
    match db.meetings().set_title(meeting_id, title) {
        Ok(()) => {
            receipt.minted_title = title.to_owned();
            report.title = Some(title.to_owned());
        }
        Err(e) => report.problems.push(format!("title: {e}")),
    }
}

/// Write one meeting's enrichment receipt.
///
/// Best-effort by the module's own rule: a stamp that could not be stored is
/// one more problem, never an error. The cost of losing one is a meeting the
/// exporter holds back until the grace window opens, which is the behaviour
/// the window exists to provide.
fn stamp(db: &mut Db, meeting_id: &str, receipt: &EnrichReceipt, report: &mut EnrichReport) {
    let mut all = read_receipts(db);
    all.insert(meeting_id.to_owned(), receipt.clone());
    match serde_json::to_string(&all) {
        Ok(json) => {
            if let Err(e) = db.put_setting(RECEIPTS_KEY, &json) {
                report.problems.push(format!("enrichment stamp: {e}"));
            }
        }
        Err(e) => report.problems.push(format!("enrichment stamp: {e}")),
    }
}

/// Enrich up to `limit` meetings that have a transcript and no summary.
///
/// Returns how many were attempted. The backfill pass behind #74: a meeting
/// that missed its one enrichment window used to stay unsummarised forever,
/// because `insert_summary` has a single production caller and nothing ever
/// looked back. Thirty-three meetings were in that state on the first real
/// library, and `fotwd summarize <id>` — which existed the whole time — is not
/// something anyone runs thirty-three times.
///
/// Timid on purpose:
///
/// * **Nothing at all without an engine.** Not even a report: re-stamping
///   `no_engine` every hour on every stranded meeting is a write per meeting
///   per hour that tells nobody anything new.
/// * **The oldest first, capped**, so one pass cannot fire off a run of CLI
///   invocations on a laptop that is also doing something else.
/// * **Never a `failed` meeting** — that exclusion lives in
///   [`needing_summary`](fotw_store::MeetingRepo::needing_summary), where the
///   reasoning is written down: retrying a usage limit hourly is how a
///   subscription burns in a loop.
pub async fn backfill_once(db: &mut Db, store: &dyn KeyStore, limit: i64) -> usize {
    if resolve_engine(store, db).is_none() {
        return 0;
    }
    let Ok(candidates) = db.meetings().needing_summary(limit) else {
        // A query that failed is a library problem, and this is a background
        // pass — the next one tries again.
        return 0;
    };
    let attempted = candidates.len();
    for meeting_id in candidates {
        enrich_meeting_with(db, store, &meeting_id).await;
    }
    attempted
}

// ------------------------------------------------ the legacy population (#88)

/// The `settings` key the one-shot legacy-title sweep marks itself done under.
///
/// A settings row on the same precedent as [`RECEIPTS_KEY`]: bookkeeping about
/// the library rather than a property of anything in it, and no migration.
pub const LEGACY_SWEEP_KEY: &str = "legacy_title_sweep";

/// How many meetings one page of the sweep reads.
///
/// Paged rather than `list(10_000, 0)` because the sweep loads each candidate's
/// transcript, and holding a whole library's rows *and* transcripts at once is
/// a spike a background task on somebody's laptop does not need to take.
const SWEEP_PAGE: i64 = 500;

/// The one-shot sweep's own receipt: when it ran and what it saw.
///
/// Tolerant on the way in like every other settings row here — a missing or
/// unparseable marker is a library that has never swept, which costs one extra
/// (idempotent, engine-free) pass and never an error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct LegacySweep {
    /// When the sweep finished, epoch milliseconds. Non-zero is what makes it
    /// done; the number itself is for whoever reads this row in a bug report.
    finished_at_ms: u64,
    /// Meetings examined.
    scanned: u64,
    /// Meetings adopted.
    adopted: u64,
}

/// What one legacy-title sweep found.
#[derive(Debug, Default)]
pub struct LegacyTitles {
    /// How many meetings it looked at.
    pub scanned: usize,
    /// Meetings still wearing a title [`fallback_title`] would mint for them
    /// today, *and* which this machine is allowed to replace. The candidates
    /// for a re-title, and the number a command must show before it spends.
    pub wearing: Vec<String>,
    /// The subset of `wearing` that had no receipt and now has one.
    pub adopted: Vec<String>,
    /// Everything that went wrong, in the order it went wrong.
    pub problems: Vec<String>,
}

/// Adopt the titles an older build minted into the receipt map — #88.
///
/// The thirty-odd meetings in the first real library are named after their
/// opening sentence, and they predate #76's map. With no receipt to match and
/// no [`FALLBACK_PREFIX`] to catch, `enrich_meeting_with`'s guard reads them as
/// human renames and freezes them: correct from what it can see, and wrong.
///
/// # Why byte-equality is a proof and not a guess
///
/// [`fallback_title`] is deterministic — the first utterance of four or more
/// words, cut to the title budget at a word boundary. Recomputing it over a
/// meeting's own stored segments and getting the stored title back *exactly*
/// means that function wrote it; nothing else in this daemon produces that
/// string, and a person renaming a meeting does not reproduce their own
/// transcript's first four-word utterance to the byte. So the classification
/// needs no heuristic, and there is no threshold to tune: it is `==`, on the
/// untrimmed, uncased bytes. A near miss — a full stop added, a different
/// utterance from the same transcript — is a rename and stays one.
///
/// # What it deliberately does not touch
///
/// * **A receipt that names a *different* title.** #76 already owns that
///   answer: a stamp saying "this machine minted X" over a title that is now Y
///   is a record of a rename, and second-guessing it is how the guard would
///   start losing renames.
///
///   A receipt whose `minted_title` is *empty* is not that. It is what every
///   pass over a legacy meeting has been leaving behind since #76 landed — the
///   pass stamped its clocks, found the title unreplaceable, and minted
///   nothing — so it carries no claim about the current title at all, and the
///   byte match is still the only evidence there is. Those are adopted too,
///   which matters more than it sounds: #74's sweeper has been running hourly,
///   and every legacy meeting it reached already has one.
/// * **A meeting still wearing its persist-time title.** It is already
///   replaceable through the prefix arm, so adoption buys nothing — and it is
///   the only way a receiptless *recent* meeting can exist, since a pass in
///   flight stamps before it does anything. Skipping it is what keeps this
///   sweep from handing the GitHub exporter a meeting inside its grace
///   window (#76).
///
/// Nothing here reaches an engine and nothing leaves the machine: it is a
/// transcript read and a string compare, which is why it can run unasked.
pub fn adopt_legacy_titles(db: &mut Db) -> LegacyTitles {
    let mut found = LegacyTitles::default();
    let mut receipts = read_receipts(db);
    let mut adopted_any = false;
    let mut offset = 0i64;

    loop {
        let page = match db.meetings().list(SWEEP_PAGE, offset) {
            Ok(page) => page,
            Err(e) => {
                // A query that failed leaves the sweep unmarked, so the next
                // one starts over: a half-swept library must not read as done.
                found.problems.push(format!("legacy titles: {e}"));
                break;
            }
        };
        let last_page = page.len() < SWEEP_PAGE as usize;
        for meeting in page {
            found.scanned += 1;
            if meeting.title.is_empty() || meeting.title.starts_with(FALLBACK_PREFIX) {
                continue;
            }
            let segments = crate::summarize::stored_segments(db, &meeting.id).unwrap_or_default();
            if fallback_title(&segments).as_deref() != Some(meeting.title.as_str()) {
                continue;
            }
            // Both clocks stay as they are — zero on a meeting no pass has
            // ever reached. They mean "when the last enrichment pass ran", and
            // this is not one. `github::export_ready` reads a clockless stamp
            // as ancient rather than as a pass in flight, which is the same
            // answer it already gave a meeting with no stamp at all.
            let receipt = receipts.entry(meeting.id.clone()).or_default();
            if receipt.minted_title == meeting.title {
                // Already classified as this machine's own work, by #76 or by
                // an earlier sweep. Replaceable, so still a re-title candidate.
                found.wearing.push(meeting.id);
            } else if receipt.minted_title.is_empty() {
                receipt.minted_title = meeting.title.clone();
                adopted_any = true;
                found.adopted.push(meeting.id.clone());
                found.wearing.push(meeting.id);
            }
            // Anything else is a receipt naming a *different* title: #76
            // classified this meeting already, and what it recorded is a
            // rename. Left alone.
        }
        if last_page {
            break;
        }
        offset += SWEEP_PAGE;
    }

    if adopted_any {
        match serde_json::to_string(&receipts) {
            Ok(json) => {
                if let Err(e) = db.put_setting(RECEIPTS_KEY, &json) {
                    found.problems.push(format!("legacy titles: {e}"));
                    found.adopted.clear();
                }
            }
            Err(e) => {
                found.problems.push(format!("legacy titles: {e}"));
                found.adopted.clear();
            }
        }
    }
    found
}

/// [`adopt_legacy_titles`], the first time a library ever asks — `None` after.
///
/// Once per library rather than once per daemon start, because the answer
/// cannot change on its own: every meeting recorded since #76 is stamped at the
/// *start* of its enrichment, so a receiptless meeting is by construction one
/// from before that, and the population only shrinks. Re-reading every
/// transcript in the library every hour to re-learn that is a cost with no
/// upside — and the meetings it would keep re-examining are precisely the
/// renames, which are the ones it must never touch.
///
/// A sweep that hit a library error is not marked done: it retries.
pub fn adopt_legacy_titles_once(db: &mut Db) -> Option<LegacyTitles> {
    let swept = db
        .get_setting(LEGACY_SWEEP_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<LegacySweep>(&v).ok())
        .is_some_and(|s| s.finished_at_ms > 0);
    if swept {
        return None;
    }

    let mut found = adopt_legacy_titles(db);
    if found.problems.is_empty() {
        let marker = LegacySweep {
            finished_at_ms: u64::try_from(fotw_store::now_ms()).unwrap_or(0),
            scanned: found.scanned as u64,
            adopted: found.adopted.len() as u64,
        };
        match serde_json::to_string(&marker) {
            Ok(json) => {
                if let Err(e) = db.put_setting(LEGACY_SWEEP_KEY, &json) {
                    found.problems.push(format!("legacy sweep marker: {e}"));
                }
            }
            Err(e) => found.problems.push(format!("legacy sweep marker: {e}")),
        }
    }
    Some(found)
}

/// Ask the engine to name each of `ids`, and say what went wrong.
///
/// The half of #88 that costs money: one title call per meeting, and nothing
/// else — not a summary, not a full enrichment pass. It is never reached by a
/// background loop. `fotwd retitle-legacy` counts the meetings and prints the
/// price first, and only `--apply` gets here (#34).
///
/// Every failure is a line and nothing more, by this module's own rule: a
/// meeting whose title call failed keeps the title it had, which is the same
/// place it started.
pub async fn retitle_meetings(db: &mut Db, store: &dyn KeyStore, ids: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    if ids.is_empty() {
        return problems;
    }
    let Some(engine) = resolve_engine(store, db) else {
        problems.push(NO_ENGINE.to_owned());
        return problems;
    };
    for meeting_id in ids {
        let segments = crate::summarize::stored_segments(db, meeting_id).unwrap_or_default();
        if segments.is_empty() {
            continue;
        }
        // Re-read rather than snapshot: the daemon may be enriching this same
        // library, and the clocks in its stamp are the exporter's grace anchor.
        let mut receipt = read_receipts(db).remove(meeting_id).unwrap_or_default();
        let mut report = EnrichReport::default();
        title_from_engine(
            db,
            &engine,
            meeting_id,
            &segments,
            &mut receipt,
            &mut report,
        )
        .await;
        if report.title.is_some() {
            stamp(db, meeting_id, &receipt, &mut report);
        }
        problems.extend(
            report
                .problems
                .into_iter()
                .map(|p| format!("{meeting_id}: {p}")),
        );
    }
    problems
}

/// Run the pipeline and say how it went, in the two codes a run can produce.
///
/// A run that succeeded *with* warnings is `ok`, not `failed`: the summary is
/// there and renders, the warning already rides inside its markdown as an
/// admonition (#75), and calling it a failure would print "Summary failed"
/// over a summary the user can read — as well as excluding it from the
/// backfill sweeper, which is right, but for the wrong reason.
async fn summarize(
    db: &mut Db,
    engine: &Engine,
    meeting_id: &str,
    report: &mut EnrichReport,
) -> (&'static str, Option<String>) {
    let mut fail = |problem: String| {
        report.problems.push(problem.clone());
        ("failed", Some(problem))
    };

    let set = match TemplateSet::load_or_builtin(default_templates_dir()) {
        Ok(set) => set,
        Err(e) => return fail(format!("templates: {e}")),
    };
    let title = db
        .meetings()
        .get(meeting_id)
        .map(|m| m.title)
        .unwrap_or_default();
    let Some(template) = set.for_event_title(&title) else {
        return fail("no templates installed — run `fotwd templates install`".to_owned());
    };

    match summarize_meeting_on(db, engine, meeting_id, template).await {
        Ok(outcome) => {
            report.summary_version = Some(outcome.version);
            // A summary that stored fine but lost its structured half is a
            // problem the user should hear about, and this report is the only
            // channel the daemon path has (#75).
            report.problems.extend(outcome.warnings);
            ("ok", None)
        }
        // Unreachable from here since #87 — the engine arrives resolved, so
        // this call has nothing left to fail to resolve. Kept because the arm
        // is a *classification*, not a code path: `no_engine` and `failed` are
        // the two states #74 split apart, and anything that put a resolve back
        // inside the run would otherwise report a missing engine as a broken
        // one without a single test noticing.
        Err(SummarizeRunError::NoKey) => {
            report.problems.push(NO_ENGINE.to_owned());
            ("no_engine", None)
        }
        Err(e) => fail(format!("summarize: {e}")),
    }
}

/// What "no engine" says.
///
/// Worded for both places it lands. It reaches the dashboard through
/// `enrich_status`, and it reaches `fotwd record`'s stderr as a `problems`
/// line — so copy that says only "see Settings" is advice about a window a
/// CLI user never opened. Both remedies, always.
const NO_ENGINE: &str = "no summarization engine is configured, so this meeting got a title but no summary — \
     turn one on in the dashboard's Settings, or run \
     `fotwd engine claude-cli --i-acknowledge-egress`";

/// What "configured, but not here" says. Names the binary: a report that will
/// not say which path failed cannot be acted on.
fn unresolvable(configured: &str) -> String {
    format!(
        "the summarization engine `{configured}` is not where this daemon can see it, so this \
         meeting got a title but no summary — set a full path in the dashboard's Settings, or \
         run `fotwd engine claude-cli --i-acknowledge-egress --binary <path>`"
    )
}

/// [`enrich_meeting_with`] against the daemon's real library and keychain.
///
/// `root` is the sessions directory, exactly as the recorder holds it.
pub async fn enrich_meeting(root: &Path, meeting_id: &str) -> EnrichReport {
    let mut report = EnrichReport::default();
    let store = match crate::secrets::keystore() {
        Ok(s) => s,
        Err(e) => {
            report.problems.push(format!("keychain: {e}"));
            return report;
        }
    };
    match crate::open_library(root) {
        Ok(mut db) => enrich_meeting_with(&mut db, store, meeting_id).await,
        Err(e) => {
            report.problems.push(format!("library: {e}"));
            report
        }
    }
}
