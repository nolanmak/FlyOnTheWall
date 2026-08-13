//! Where the retention engine is actually connected (§9.5, issue #41).
//!
//! Three separate things existed and none of them met: `fotw-pipeline`'s
//! sweeper knew what to delete but was never called, `fotw-pipeline`'s Opus
//! encoder knew how to archive a session but nothing moved the result into
//! `media/`, and `fotw-store`'s `recordings` table had been in the schema
//! since migration 0001 with nothing writing a row to it. So audio accumulated
//! in `sessions/` forever, `media/` was never created, and the feature that
//! was supposed to stop a daily recorder filling their disk had never once
//! run.
//!
//! This module is the wiring, and it is deliberately thin: every decision
//! worth testing lives upstream in a pure function.
//!
//! # Promotion
//!
//! [`promote_session`] runs [`fotw_pipeline::promote`] and then writes the
//! `recordings` rows. The order is forced and not arbitrary — the row points
//! at a file, so the file has to exist first. A crash between them leaves
//! media on disk that the library does not know about, which
//! [`resume_promotions`] finds and finishes at the next start. The reverse
//! order would leave the library pointing at audio that does not exist, which
//! nothing can repair.
//!
//! # Sweeping
//!
//! [`sweep`] turns the library into `Vec<MeetingAudio>`, hands it to
//! [`plan_sweep`], and — only in [`SweepMode::Apply`] — unlinks. The plan is
//! reported either way, because deleting a user's meeting audio is
//! irreversible and "here is what would happen" has to be available before it
//! does.
//!
//! # Scheduling
//!
//! [`Schedule`] is a pure state machine over `(now, is_recording)`, so "does
//! it run at startup, hourly, and never during a capture" is three assertions
//! rather than a test that sleeps for an hour. **It never sweeps while a
//! recording is in flight**: competing for disk I/O with a live capture is how
//! buffers get dropped, and the sweep can always wait.

use std::path::{Path, PathBuf};
use std::time::Duration;

use fotw_pipeline::promote::{self, PromoteError, Promotion};
use fotw_pipeline::retention::{
    DiskUsage, MeetingAudio, RetentionPolicy, RetentionSettings, SweepPlan, SweepWarning,
    TranscriptState, plan_sweep, usage,
};
use fotw_store::{Db, NewRecording};

/// The `settings` key the budget and window live under (§9.3).
pub const SETTINGS_KEY: &str = "retention";

/// Issue #41: "background sweeper on app start and hourly".
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(3_600);

/// How long after its last write an unfinalized session stops counting as a
/// live recording.
///
/// A session that crashed never gets its `ended_at_ms`, so "unfinalized" alone
/// would mean one crash disables retention permanently. "Unfinalized *and*
/// written to recently" degrades to the right answer on its own: the pump
/// syncs every two seconds (`wal::SYNC_INTERVAL`), so a minute of silence from
/// a session is thirty missed syncs and not a slow disk.
pub const LIVE_SESSION_WINDOW_MS: u64 = 60_000;

/// Whether a sweep is allowed to delete anything.
///
/// A type rather than a boolean argument because the destructive path should
/// be unmistakable at the call site. The CLI defaults to [`SweepMode::DryRun`]
/// for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepMode {
    /// Decide and report. Nothing is unlinked, no row is touched.
    DryRun,
    /// Decide, report, and carry it out.
    Apply,
}

impl SweepMode {
    /// Whether this mode deletes.
    #[must_use]
    pub const fn deletes(self) -> bool {
        matches!(self, Self::Apply)
    }
}

/// What one sweep decided and, if it was allowed to, did.
#[derive(Debug, Clone)]
pub struct SweepReport {
    /// Whether this run was allowed to delete.
    pub mode: SweepMode,
    /// The clock the plan was made against.
    pub now_ms: u64,
    /// The settings in force.
    pub settings: RetentionSettings,
    /// Disk accounting before the sweep.
    pub usage: DiskUsage,
    /// What the sweeper decided.
    pub plan: SweepPlan,
    /// Bytes actually reclaimed. Always zero in a dry run.
    pub bytes_reclaimed: u64,
    /// Files actually unlinked. Always zero in a dry run.
    pub files_removed: usize,
    /// Failures, reported rather than raised: one unreadable file must not
    /// leave the rest of the library over budget.
    pub errors: Vec<String>,
}

impl SweepReport {
    /// How many files this plan covers.
    #[must_use]
    pub fn would_delete(&self) -> usize {
        self.plan.evictions.iter().map(|e| e.paths.len()).sum()
    }

    /// A human-readable report, naming every file.
    ///
    /// Loud on purpose. This is irreversible deletion of the one artifact a
    /// user cannot regenerate, so the output says what went, why, how much it
    /// freed, and — the part that is easiest to leave out and most important
    /// to include — what it deliberately did *not* touch.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "  audio      : {} in {} meetings (budget {}, {:.0}% used)",
            human(self.usage.audio_bytes),
            self.usage.meetings,
            human(self.settings.budget_bytes),
            self.usage.budget_fraction(self.settings.budget_bytes) * 100.0
        );
        let _ = writeln!(
            s,
            "  transcripts: {} — never subject to retention",
            human(self.usage.transcript_bytes)
        );
        let _ = writeln!(
            s,
            "  protected  : {} kept forever, {} not yet transcribed",
            human(self.usage.forever_bytes),
            human(self.usage.untranscribed_bytes)
        );
        let _ = writeln!(
            s,
            "  policy     : delete {} days after the transcript is ready",
            self.settings.default_days
        );
        let _ = writeln!(s);

        if self.plan.evictions.is_empty() {
            let _ = writeln!(s, "  Nothing is due. No audio would be deleted.");
        } else {
            let verb = if self.mode.deletes() {
                "deleted"
            } else {
                "would be deleted"
            };
            let _ = writeln!(
                s,
                "  {} meeting(s), {} {verb}:",
                self.plan.evictions.len(),
                human(self.plan.bytes_reclaimed())
            );
            for ev in &self.plan.evictions {
                let _ = writeln!(
                    s,
                    "    {} — {} ({})",
                    ev.meeting_id,
                    reason_text(&ev.reason),
                    human(ev.audio_bytes)
                );
                for p in &ev.paths {
                    let _ = writeln!(s, "      {}", p.display());
                }
            }
        }

        for w in &self.plan.warnings {
            let _ = writeln!(s);
            match w {
                SweepWarning::OnlyForeverRemains {
                    over_by_bytes,
                    forever_bytes,
                } => {
                    let _ = writeln!(
                        s,
                        "  ! {} over budget, and the {} still on disk is \
                         marked Keep forever.",
                        human(*over_by_bytes),
                        human(*forever_bytes)
                    );
                    let _ = writeln!(
                        s,
                        "    Nothing was evicted: `forever` means forever. Raise the \
                         budget or change a meeting's setting."
                    );
                }
                SweepWarning::OnlyUntranscribedRemains {
                    over_by_bytes,
                    untranscribed_bytes,
                } => {
                    let _ = writeln!(
                        s,
                        "  ! {} over budget, and the {} still on disk has \
                         not been transcribed.",
                        human(*over_by_bytes),
                        human(*untranscribed_bytes)
                    );
                    let _ = writeln!(
                        s,
                        "    That audio is the ONLY copy of those meetings, so it was \
                         left alone. Transcribe them, or raise the budget."
                    );
                }
            }
        }

        if !self.errors.is_empty() {
            let _ = writeln!(s);
            for e in &self.errors {
                let _ = writeln!(s, "  ! {e}");
            }
        }

        let _ = writeln!(s);
        if self.mode.deletes() {
            let _ = writeln!(
                s,
                "  reclaimed  : {} in {} file(s)",
                human(self.bytes_reclaimed),
                self.files_removed
            );
        } else {
            let _ = writeln!(
                s,
                "  dry run — nothing was deleted. Re-run with --apply to carry this out."
            );
        }
        s
    }
}

/// Bytes at a scale a human can read.
///
/// Not always GiB: a report that says "0.00 GiB would be deleted" while four
/// megabytes of a user's meeting goes is technically true and useless, and the
/// entire point of the dry run is that the user can see what is about to
/// happen.
fn human(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.2} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn reason_text(reason: &fotw_pipeline::retention::SweepReason) -> String {
    use fotw_pipeline::retention::SweepReason as R;
    match reason {
        R::PolicyAge { days } => format!("past its {days}-day retention window"),
        R::Transcribed => "kept only until transcribed".to_owned(),
        R::OverBudget => "oldest audio over the disk budget".to_owned(),
    }
}

// ------------------------------------------------------------------ promotion

/// Promote a finished session into `media/` and record it in the library.
///
/// Files first, rows second: the row points at the file. See the module docs
/// for why the reverse ordering is unrepairable.
///
/// # Errors
///
/// Anything [`fotw_pipeline::promote`] or the store rejects, as a message. The
/// session directory survives every failure.
pub fn promote_session(
    db: &mut Db,
    data_root: &Path,
    session_dir: &Path,
    meeting_id: &str,
    started_at_ms: u64,
) -> Result<Promotion, String> {
    promote::claim(session_dir, meeting_id, started_at_ms).map_err(|e| e.to_string())?;
    let promoted = promote::promote(session_dir, data_root).map_err(|e| e.to_string())?;
    record(db, &promoted)?;
    Ok(promoted)
}

/// Finish every promotion an earlier run left half-done.
///
/// Run at daemon start. Returns one result per session so a single failure is
/// reported rather than aborting the rest.
#[must_use]
pub fn resume_promotions(db: &mut Db, data_root: &Path) -> Vec<Result<Promotion, String>> {
    promote::resume(sessions_dir(data_root), data_root)
        .into_iter()
        .map(|r| {
            let promoted = r.map_err(|e: PromoteError| e.to_string())?;
            record(db, &promoted)?;
            Ok(promoted)
        })
        .collect()
}

/// Write the `recordings` rows for a promotion.
fn record(db: &mut Db, promoted: &Promotion) -> Result<(), String> {
    for t in &promoted.tracks {
        db.upsert_recording(
            &promoted.meeting_id,
            &NewRecording {
                channel: t.channel.clone(),
                rel_path: t.rel_path.clone(),
                bytes: t.bytes,
                duration_ms: t.duration_ms,
                sample_rate_hz: t.sample_rate_hz,
            },
        )
        .map_err(|e| format!("recording {}: {e}", t.rel_path))?;
    }
    Ok(())
}

// ------------------------------------------------------------------- settings

/// The retention settings in force, falling back to §9.5's defaults.
///
/// A missing or unparseable value is the defaults, never "retention off". The
/// failure mode of guessing wrong in that direction is a disk that silently
/// fills, which is the entire problem this feature exists to solve.
#[must_use]
pub fn settings(db: &Db) -> RetentionSettings {
    db.get_setting(SETTINGS_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Persist the retention settings.
///
/// # Errors
///
/// Propagates store failures.
pub fn set_settings(db: &mut Db, s: &RetentionSettings) -> Result<(), String> {
    let json = serde_json::to_string(s).map_err(|e| e.to_string())?;
    db.put_setting(SETTINGS_KEY, &json)
        .map_err(|e| e.to_string())
}

// ------------------------------------------------------------------- sweeping

/// The library, as the sweeper sees it.
///
/// `bytes` falls back to a `stat` where the column is NULL — a row written
/// before sizes were recorded, or restored from an archive. Treating an
/// unknown size as zero would make that audio invisible to the budget, which
/// is the failure mode where the disk fills while the settings screen reports
/// plenty of room.
///
/// # Errors
///
/// Propagates store failures.
pub fn inventory(db: &Db, data_root: &Path) -> Result<Vec<MeetingAudio>, String> {
    let rows = db.audio_inventory().map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut audio_bytes = 0u64;
            let mut audio_paths = Vec::with_capacity(r.audio.len());
            for f in &r.audio {
                let bytes = f.bytes.or_else(|| {
                    std::fs::metadata(data_root.join(&f.rel_path))
                        .ok()
                        .map(|m| m.len())
                });
                audio_bytes += bytes.unwrap_or(0);
                audio_paths.push(PathBuf::from(&f.rel_path));
            }
            MeetingAudio {
                meeting_id: r.meeting_id,
                started_at_ms: r.started_at_ms.max(0) as u64,
                state: TranscriptState::from_column(&r.state),
                transcript_ready_at_ms: r
                    .transcript_ready_at_ms
                    .and_then(|t| u64::try_from(t).ok()),
                policy: RetentionPolicy::from_columns(
                    &r.retain_audio,
                    r.retain_audio_days.and_then(|d| u32::try_from(d).ok()),
                ),
                audio_bytes,
                transcript_bytes: r.transcript_bytes,
                audio_paths,
            }
        })
        .collect())
}

/// Decide what to delete, report it, and — in [`SweepMode::Apply`] — do it.
///
/// `now_ms` is a parameter rather than a call to the clock so the whole thing
/// is testable at any point in a retention window without waiting for one.
///
/// # Errors
///
/// Propagates store failures. Filesystem failures during the unlink are
/// collected into [`SweepReport::errors`] instead: one unreadable file must
/// not leave the rest of the library over budget.
pub fn sweep(
    db: &mut Db,
    data_root: &Path,
    now_ms: u64,
    mode: SweepMode,
) -> Result<SweepReport, String> {
    let settings = settings(db);
    let meetings = inventory(db, data_root)?;
    let usage = usage(&meetings);
    let plan = plan_sweep(now_ms, &meetings, &settings);

    // Refresh the per-meeting deadline whichever mode this is: the settings
    // screen's "audio goes on <date>" comes from it, and a dry run is exactly
    // when a user is looking at that screen.
    let mut errors = Vec::new();
    for m in &meetings {
        let deadline = m
            .expires_at_ms(settings.default_days)
            .and_then(|d| i64::try_from(d).ok());
        if let Err(e) = db.set_purge_after(&m.meeting_id, deadline) {
            errors.push(format!("recording deadline for {}: {e}", m.meeting_id));
        }
    }

    let (mut bytes_reclaimed, mut files_removed) = (0u64, 0usize);
    if mode.deletes() {
        let (reclaimed, failures) = plan.apply(data_root);
        bytes_reclaimed = reclaimed;
        for (path, e) in failures {
            errors.push(format!("unlinking {}: {e}", path.display()));
        }
        for ev in &plan.evictions {
            files_removed += ev.paths.len();
            // The row outlives the bytes so the library can say "audio was
            // deleted on <date>" rather than pretending the meeting never had
            // any (§9.3's note on `recordings.deleted_at`).
            if let Err(e) =
                db.mark_audio_deleted(&ev.meeting_id, i64::try_from(now_ms).unwrap_or(i64::MAX))
            {
                errors.push(format!("retiring rows for {}: {e}", ev.meeting_id));
            }
        }
        files_removed = files_removed.saturating_sub(
            errors
                .iter()
                .filter(|e| e.starts_with("unlinking "))
                .count(),
        );
    }

    Ok(SweepReport {
        mode,
        now_ms,
        settings,
        usage,
        plan,
        bytes_reclaimed,
        files_removed,
        errors,
    })
}

// ----------------------------------------------------------------- scheduling

/// What a scheduler poll decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Sweep now.
    Run,
    /// Not due yet.
    Waiting {
        /// Milliseconds until it is.
        in_ms: u64,
    },
    /// Due, but a recording is in flight. Deliberately does not consume the
    /// turn — the sweep runs as soon as the recording ends.
    HeldForRecording,
}

/// When to sweep.
///
/// A pure state machine so the policy is testable without a clock: "on start,
/// then hourly, and never during a capture" is three assertions rather than an
/// hour of sleeping.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    interval_ms: u64,
    last_run_ms: Option<u64>,
}

impl Schedule {
    /// Issue #41's cadence: at start, then every hour.
    #[must_use]
    pub const fn hourly() -> Self {
        Self::every(SWEEP_INTERVAL)
    }

    /// A custom cadence, for tests and for a user who wants it rarer.
    #[must_use]
    pub const fn every(interval: Duration) -> Self {
        Self {
            interval_ms: interval.as_millis() as u64,
            last_run_ms: None,
        }
    }

    /// Decide, and record the decision.
    ///
    /// `recording` is the veto and it is checked first, before "is it due":
    /// the answer to "may I do disk I/O right now" cannot depend on how long
    /// it has been since the last sweep.
    pub fn poll(&mut self, now_ms: u64, recording: bool) -> Tick {
        if recording {
            return Tick::HeldForRecording;
        }
        match self.last_run_ms {
            None => {
                self.last_run_ms = Some(now_ms);
                Tick::Run
            }
            Some(last) => {
                let elapsed = now_ms.saturating_sub(last);
                if elapsed >= self.interval_ms {
                    self.last_run_ms = Some(now_ms);
                    Tick::Run
                } else {
                    Tick::Waiting {
                        in_ms: self.interval_ms - elapsed,
                    }
                }
            }
        }
    }
}

/// Whether a capture is writing to disk right now.
///
/// Filesystem-derived rather than an in-process flag, so it is honest across
/// the daemon, the CLI, and the shell — three processes that can all be
/// holding the same data root. A session counts as live when it is
/// unfinalized (§5.4: `ended_at_ms` absent) *and* something wrote to it within
/// [`LIVE_SESSION_WINDOW_MS`].
#[must_use]
pub fn recording_in_flight(sessions_root: &Path, now_ms: u64) -> bool {
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return false;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // Cheap and dependency-free: `manifest.json` holding no `ended_at_ms`
        // is the §5.4 recovery signal, and it is a small file.
        let Ok(text) = std::fs::read_to_string(dir.join("manifest.json")) else {
            continue;
        };
        if text.contains("\"ended_at_ms\"") {
            continue;
        }
        if newest_write_ms(&dir).is_some_and(|t| now_ms.saturating_sub(t) <= LIVE_SESSION_WINDOW_MS)
        {
            return true;
        }
    }
    false
}

fn newest_write_ms(dir: &Path) -> Option<u64> {
    let mut newest = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let ms = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        newest = Some(newest.map_or(ms, |n: u64| n.max(ms)));
    }
    newest
}

/// `<root>/sessions`, the live session tree of §9.2.
#[must_use]
pub fn sessions_dir(data_root: &Path) -> PathBuf {
    data_root.join("sessions")
}
