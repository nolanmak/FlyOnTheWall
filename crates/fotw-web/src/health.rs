//! `GET /api/health` — the daemon's own state, read-only (#101).
//!
//! Every other control in this crate *changes* something: the recorder starts
//! a meeting, the exporter pushes one, the summarize control rewrites
//! settings. This one only answers, and what it answers is the question a
//! person actually asks of a daemon they cannot see: is it alive, what engine
//! did it resolve, what has it done in the last hour, and how much work is
//! still queued.
//!
//! It exists because that question had no answer. Asked on 2026-08-25 whether
//! summarization was working, the only way to find out was to kill the running
//! daemon and relaunch it in a terminal — the daemon's stderr belongs to a
//! LaunchServices-launched `.app`, which macOS discards. Three wrong
//! conclusions were drawn from the outside before anyone did.
//!
//! # Why every field can be absent, and why that matters
//!
//! `None` here means *this has not happened yet*, never *this happened and
//! amounted to nothing*. Those two were the same observation — silence — and
//! collapsing them is the exact defect this endpoint was added to remove, so
//! it must not be reintroduced by a serializer that writes `0` for a pass that
//! never ran.
//!
//! # What may be in it
//!
//! Ids, counts, clocks, an engine name and a file path. No titles, no
//! transcript text, no note text, no key material — §10's never-log rule reads
//! on this surface exactly as it reads on the journal, and for a stronger
//! reason: this one crosses HTTP.

use serde::{Deserialize, Serialize};

/// The last time a background loop did its thing, and what came of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// When it finished, epoch milliseconds.
    pub at_ms: u64,
    /// One line, already rendered. Prose rather than a struct per loop,
    /// because each loop's interesting numbers are different and the reader is
    /// a person: a schema that could hold all three would say less than the
    /// sentence each of them already writes into the journal.
    pub summary: String,
}

/// `GET /api/health`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
    /// When this daemon started, epoch milliseconds. Its uptime, and the
    /// answer to "did it restart while I was away".
    pub started_at_ms: u64,
    /// What the summarize engine resolves to on this machine right now —
    /// `none`, `unresolvable (codex)`, or the engine and its binary. #74's
    /// three states, which used to look identical from outside.
    pub engine: String,
    /// Meetings with a transcript, no summary, and no failed attempt: the
    /// depth of the backfill queue this instant, not as of the last pass.
    pub awaiting_enrichment: u64,
    /// The last enrichment backfill pass. `None` until one has run.
    pub backfill: Option<Activity>,
    /// The last GitHub auto-push round. `None` until one has run.
    pub github: Option<Activity>,
    /// The last retention sweep. `None` until one has run.
    pub retention: Option<Activity>,
    /// Where the daemon's log is, for the detail this summary leaves out.
    /// `None` when it could not be opened — which is itself worth seeing.
    pub log_path: Option<String>,
}

/// What the daemon tells the dashboard about itself.
///
/// One method, deliberately: a read-only surface with a setter would stop
/// being one.
pub trait DaemonHealth: Send + Sync {
    /// The daemon's state right now.
    ///
    /// May touch the library, so callers run it off the runtime — see the
    /// `spawn_blocking` rule in [`crate::api`].
    fn report(&self) -> HealthReport;
}
