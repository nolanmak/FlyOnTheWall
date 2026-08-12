//! The local audit log (CON-08), and the record CON-01 is stated against.
//!
//! > Acceptance: a fresh install never writes an audio buffer to disk without
//! > a user-initiated Start event in the local audit log.
//! >
//! > — docs/REQUIREMENTS.md 11.2 CON-01
//!
//! So this is not logging. It is the artifact that answers "who started this
//! recording, and what were they told first" — the direct response to the
//! *Otter.ai* theory that consent was outsourced to the customer rather than
//! built into the product.
//!
//! # Shape
//!
//! Append-only JSONL beside the session directory, one object per line, and
//! **never** rewritten. A line-per-event file survives a `kill -9` mid-write
//! with at most one damaged trailing line, which a rewritten JSON array does
//! not.
//!
//! It is local, and it stays local: nothing in this crate sends it anywhere,
//! and it holds no audio, no transcript text and no attendee data — only the
//! fact that a person pressed record and what warning they saw when they did.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One line of the log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEvent {
    /// Wall-clock time, milliseconds since the Unix epoch.
    ///
    /// Wall clock rather than the monotonic clock the shell runs on: this is a
    /// record for a human reading it weeks later, and "1.2 s after an
    /// arbitrary epoch" answers no question anyone will ask of it.
    pub at_unix_ms: u64,
    /// What happened.
    #[serde(flatten)]
    pub kind: AuditKind,
}

/// The events worth keeping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditKind {
    /// A person started a recording.
    SessionStart {
        /// Which human action started it — [`fotw_shell::StartOrigin::label`].
        ///
        /// A string rather than the enum so the file format survives the enum
        /// changing shape. There is deliberately no value here meaning
        /// "automatic": see CON-01.
        origin: String,
        /// The app the detection prompt named, when the start came from one.
        detected_app: Option<String>,
        /// The jurisdiction warning the user was shown (CON-05).
        jurisdiction_warning: String,
        /// Whether that warning was blocking and had to be acknowledged.
        acknowledged_all_party: bool,
    },
    /// A recording ended.
    SessionEnd {
        /// The session directory, so the log can be reconciled with disk.
        session: String,
        /// How long it ran.
        duration_ms: u64,
    },
    /// A detection prompt was raised and *not* acted on. Kept because "we
    /// showed a warning and the user declined" is as much a consent record as
    /// a start.
    DetectionDeclined {
        /// Which app was detected.
        app: String,
        /// `not_now` or `never_for_this_app`.
        answer: String,
    },
}

/// An append-only audit log.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// The log beside a sessions root.
    #[must_use]
    pub fn at(sessions_root: &Path) -> Self {
        let dir = sessions_root.parent().unwrap_or(sessions_root);
        Self {
            path: dir.join("audit.jsonl"),
        }
    }

    /// Where it lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event, stamped now.
    ///
    /// # Errors
    ///
    /// If the file cannot be opened, written or flushed.
    pub fn record(&self, kind: AuditKind) -> io::Result<()> {
        self.record_at(now_ms(), kind)
    }

    /// Append one event with an explicit timestamp.
    ///
    /// # Errors
    ///
    /// If the file cannot be opened, written or flushed.
    pub fn record_at(&self, at_unix_ms: u64, kind: AuditKind) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let event = AuditEvent { at_unix_ms, kind };
        let mut line = serde_json::to_string(&event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        // Flushed rather than left to the page cache: the next thing that
        // happens is a tap opening, and a crash there must not lose the
        // record of who asked for it.
        file.sync_data()
    }

    /// Read the whole log.
    ///
    /// A damaged trailing line — the `kill -9` case — is skipped rather than
    /// failing the read, because a log that cannot be opened is worth less
    /// than a log missing its last line.
    ///
    /// # Errors
    ///
    /// If the file exists but cannot be read.
    pub fn read(&self) -> io::Result<Vec<AuditEvent>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
