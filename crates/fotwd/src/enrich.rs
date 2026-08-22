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
//! The fallback title. Nothing leaves the machine: `resolve_engine` returned
//! `None`, so there is nowhere for it to go — and a transcriptless meeting
//! (recorded with no provider) is left entirely alone, because silence is a
//! normal state, not a fault.

use std::path::Path;

use fotw_secrets::KeyStore;
use fotw_store::Db;
use fotw_summarize::template::{TemplateSet, default_templates_dir};

use crate::engine::{fallback_title, resolve_engine};
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

    // The summary, when an engine exists. Its markdown is versioned and
    // append-only (§8.9); the action items ride inside it, extracted and
    // evidence-validated by the pipeline's Call B.
    if !segments.is_empty() && resolve_engine(store, db).is_some() {
        match TemplateSet::load_or_builtin(default_templates_dir()) {
            Err(e) => report.problems.push(format!("templates: {e}")),
            Ok(set) => {
                let title = db
                    .meetings()
                    .get(meeting_id)
                    .map(|m| m.title)
                    .unwrap_or_default();
                match set.for_event_title(&title) {
                    None => report
                        .problems
                        .push("no templates installed — run `fotwd templates install`".to_owned()),
                    Some(template) => {
                        match summarize_meeting(db, store, meeting_id, template).await {
                            Ok(outcome) => report.summary_version = Some(outcome.version),
                            // NoKey here means the engine vanished between
                            // resolve and run — a race worth naming, not
                            // worth failing differently over.
                            Err(SummarizeRunError::NoKey) => {}
                            Err(e) => report.problems.push(format!("summarize: {e}")),
                        }
                    }
                }
            }
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
