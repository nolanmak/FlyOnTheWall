//! What the daemon says about itself — issue #101's `GET /api/health`.
//!
//! [`crate::journal`] is the narrative; this is the summary. They answer the
//! same question at two grains, and both exist because neither is enough on
//! its own: the log tells you what happened at 14:03 and the summary tells you
//! whether anything is happening *now*, which is what someone asks first.
//!
//! # Why it is a snapshot in memory rather than a query
//!
//! "The last backfill pass and its outcome" is not a fact about the library —
//! it is a fact about this process, and it stops being true when the process
//! restarts. Deriving it by parsing the journal back would mean a log format
//! that is also a wire format, which is how a file meant for a human acquires
//! a schema nobody may change. So the loops report as they finish, and a
//! daemon that has just started honestly says it has done nothing yet.
//!
//! The one exception is the queue depth, which is read from the library on
//! every request: it changes whenever a meeting ends, and the person asking
//! has usually just finished the meeting they are asking about.
//!
//! # §10
//!
//! Ids, counts, clocks, an engine name and a file path. Nothing here reads a
//! title, a transcript or a note, and the rule is tighter than the journal's
//! because this one crosses HTTP.

use std::path::PathBuf;
use std::sync::Mutex;

use fotw_store::Db;
use fotw_web::{Activity, DaemonHealth, HealthReport};

use crate::enrich::BackfillPass;

/// The daemon's own state, as the dashboard may ask for it.
#[derive(Debug)]
pub struct Health {
    started_at_ms: u64,
    log_path: Option<PathBuf>,
    /// Set when the backfill task starts and refreshed by every pass. Not
    /// resolved per request: on the Anthropic arm that is a keychain read
    /// through a store whose five-second timeout exists because those can
    /// block (#87), and a polling dashboard would make one per poll.
    engine: Mutex<String>,
    backfill: Mutex<Option<Activity>>,
    github: Mutex<Option<Activity>>,
    retention: Mutex<Option<Activity>>,
    /// Its own library connection, for the queue depth alone.
    ///
    /// The fourth in this daemon, on the same terms as the sweeper's and the
    /// exporter's: the UI's `Db` is sealed inside `StoreSource`'s mutex with no
    /// accessor, and a reader that runs when a person opens a pane is exactly
    /// what WAL mode makes free.
    queue: Mutex<Db>,
}

impl Health {
    /// A daemon that has just started and done nothing yet.
    #[must_use]
    pub fn new(log_path: Option<PathBuf>, queue: Db) -> Self {
        Self {
            started_at_ms: now_ms(),
            log_path,
            engine: Mutex::new("unknown".to_owned()),
            backfill: Mutex::new(None),
            github: Mutex::new(None),
            retention: Mutex::new(None),
            queue: Mutex::new(queue),
        }
    }

    /// What the engine resolved to, before the first pass has run.
    ///
    /// Called once when the backfill task starts, so the answer to the most
    /// common question is right immediately rather than an hour later.
    pub fn note_engine(&self, engine: &str) {
        *self.engine.lock().unwrap_or_else(|e| e.into_inner()) = engine.to_owned();
    }

    /// A finished backfill pass, whatever it found.
    ///
    /// Unconditional — a pass that enriched nothing still replaces "no pass has
    /// run", which is the distinction the whole issue turns on.
    pub fn note_backfill(&self, pass: &BackfillPass) {
        self.note_engine(&pass.engine);
        *self.backfill.lock().unwrap_or_else(|e| e.into_inner()) = Some(Activity {
            at_ms: now_ms(),
            summary: pass.summary(),
        });
    }

    /// A finished GitHub push round.
    pub fn note_github(&self, summary: &str) {
        *self.github.lock().unwrap_or_else(|e| e.into_inner()) = Some(Activity {
            at_ms: now_ms(),
            summary: summary.to_owned(),
        });
    }

    /// A finished retention sweep.
    pub fn note_retention(&self, summary: &str) {
        *self.retention.lock().unwrap_or_else(|e| e.into_inner()) = Some(Activity {
            at_ms: now_ms(),
            summary: summary.to_owned(),
        });
    }
}

impl DaemonHealth for Health {
    fn report(&self) -> HealthReport {
        HealthReport {
            started_at_ms: self.started_at_ms,
            engine: self
                .engine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            // A count that could not be read reports as zero rather than
            // failing the request: the rest of this report is still the answer
            // somebody needs, and a 500 here would look exactly like the
            // daemon being dead — which is the confusion this endpoint exists
            // to end.
            awaiting_enrichment: self
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .meetings()
                .count_needing_summary()
                .ok()
                .and_then(|n| u64::try_from(n).ok())
                .unwrap_or_default(),
            backfill: self
                .backfill
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            github: self
                .github
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            retention: self
                .retention
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            log_path: self.log_path.as_ref().map(|p| p.display().to_string()),
        }
    }
}

fn now_ms() -> u64 {
    u64::try_from(fotw_store::now_ms()).unwrap_or_default()
}
