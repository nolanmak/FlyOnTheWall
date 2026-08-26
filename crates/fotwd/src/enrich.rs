//! What a meeting gets the moment it finalizes — #67, #68.
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
//! # What runs with no engine at all
//!
//! The fallback title. Nothing leaves the machine: there is nowhere for it to
//! go — and a transcriptless meeting (recorded with no provider) is left
//! entirely alone, because silence is a normal state, not a fault.
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

use std::path::Path;

use fotw_secrets::KeyStore;
use fotw_store::Db;
use fotw_summarize::template::{TemplateSet, default_templates_dir};

use crate::engine::{EngineResolution, fallback_title, resolve_engine, resolve_engine_detailed};
use crate::summarize::{SummarizeRunError, summarize_meeting};

/// The prefix every machine-minted title carries, and the only titles
/// enrichment may replace. A rename typed by a human is never fair game,
/// which is what makes re-running enrichment safe.
const FALLBACK_PREFIX: &str = "Untitled recording";

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

    // No transcript is a normal state — recorded with no provider — and an
    // empty one gets the same treatment.
    let segments = crate::summarize::stored_segments(db, meeting_id).unwrap_or_default();

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
            EngineResolution::Engine(_) => summarize(db, store, meeting_id, &mut report).await,
        };
        // Best-effort, per the module docs: a report that could not be stored
        // is one more problem, never an error.
        if let Err(e) = db
            .meetings()
            .set_enrich_report(meeting_id, status, detail.as_deref())
        {
            report.problems.push(format!("enrich report: {e}"));
        }
    }

    // The title, last, so a summary-derived improvement could slot in here
    // later without reordering. Only the machine's own fallback is replaced.
    let current = db
        .meetings()
        .get(meeting_id)
        .map(|m| m.title)
        .unwrap_or_default();
    if current.starts_with(FALLBACK_PREFIX) {
        if let Some(title) = fallback_title(&segments) {
            match db.meetings().set_title(meeting_id, &title) {
                Ok(()) => report.title = Some(title),
                Err(e) => report.problems.push(format!("title: {e}")),
            }
        }
    } else {
        report.title = Some(current);
    }

    report
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

/// Run the pipeline and say how it went, in the two codes a run can produce.
///
/// A run that succeeded *with* warnings is `ok`, not `failed`: the summary is
/// there and renders, the warning already rides inside its markdown as an
/// admonition (#75), and calling it a failure would print "Summary failed"
/// over a summary the user can read — as well as excluding it from the
/// backfill sweeper, which is right, but for the wrong reason.
async fn summarize(
    db: &mut Db,
    store: &dyn KeyStore,
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

    match summarize_meeting(db, store, meeting_id, template).await {
        Ok(outcome) => {
            report.summary_version = Some(outcome.version);
            // A summary that stored fine but lost its structured half is a
            // problem the user should hear about, and this report is the only
            // channel the daemon path has (#75).
            report.problems.extend(outcome.warnings);
            ("ok", None)
        }
        // NoKey here means the engine vanished between resolve and run — a
        // race worth naming, not worth failing differently over.
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
