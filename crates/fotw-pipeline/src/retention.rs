//! Audio retention, the disk budget, and the sweeper (§9.5).
//!
//! A meeting recorder that keeps everything forever is a meeting recorder that
//! eventually eats the disk. §9.5 does the arithmetic: two 24 kbps mono Opus
//! tracks are 21.6 MB/hour, and a heavy user — five meetings a day, 45 minutes
//! each, 250 working days — generates **20.3 GB a year**. That is the number
//! that forces a policy.
//!
//! # The four rules, and why each exists
//!
//! 1. **Default: delete audio 30 days after the transcript reaches `ready`.**
//!    Not 30 days after the meeting — after the *transcript*, because the
//!    audio's remaining job at that point is dispute resolution ("did they
//!    really say that?"), and that window is weeks, not years. The steady
//!    state this produces, ≈1.7 GB, is what makes the default defensible;
//!    [`UsageProfile::steady_state_bytes`] computes it so the claim is a test
//!    rather than a sentence in a document.
//! 2. **Per-meeting override** — `default | forever | until_transcribed |
//!    days(n)` — because the one board meeting a year that matters should not
//!    be governed by the same rule as a standup.
//! 3. **A global byte budget** (default 20 GiB) with oldest-first eviction,
//!    because rule 1 bounds the *steady* state and not the burst.
//! 4. **Transcripts are never subject to retention.** Text is ~250 MB/year
//!    including the FTS index — it is not the problem, and deleting it would
//!    destroy the only artifact the user actually reads. Nothing in this
//!    module can name a transcript: [`SweepPlan`] carries audio paths only,
//!    and `transcript_bytes` exists purely so the accounting can prove it went
//!    untouched.
//!
//! # Why the decision is a pure function
//!
//! [`plan_sweep`] takes `(now, meetings, settings)` and returns a plan. It
//! touches no clock and no filesystem, so "what does the sweeper do to a
//! library that is 3 GB over budget on the 31st day" is an ordinary unit test
//! rather than a fixture of real files and a mocked clock. Deletion is a
//! separate, deliberately boring step ([`SweepPlan::apply`]).
//!
//! # Where this deviates from §9.5, and why
//!
//! §9.5 says eviction "skips `forever` meetings". It is silent on meetings
//! whose transcript is not `ready` yet. Evicting those would destroy the
//! source before anything derived from it existed — the one irreversible
//! mistake available here — so they are skipped too, with their own warning.
//! See [`SweepWarning`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Milliseconds in a day. Timestamps everywhere in this project are Unix epoch
/// milliseconds (§9.3).
pub const MS_PER_DAY: u64 = 86_400_000;

/// The default retention window, in days after the transcript reaches `ready`.
pub const DEFAULT_RETAIN_DAYS: u32 = 30;

/// The default global audio budget: 20 GiB.
pub const DEFAULT_BUDGET_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// What to do with one meeting's audio (the `retain_audio` column, §9.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetentionPolicy {
    /// Follow the global default: delete N days after the transcript is ready.
    #[default]
    Default,
    /// Never delete, and never evict for budget. The escape hatch that makes
    /// the aggressive default acceptable.
    Forever,
    /// Delete as soon as the transcript is ready. For users who want the text
    /// and nothing else.
    UntilTranscribed,
    /// Delete `days` after the transcript is ready.
    Days {
        /// The window.
        days: u32,
    },
}

impl RetentionPolicy {
    /// Render to the `(retain_audio, retain_audio_days)` column pair of §9.3.
    ///
    /// The schema splits the discriminant from its payload across two columns,
    /// so the conversion lives here rather than being open-coded at every
    /// query site.
    #[must_use]
    pub const fn to_columns(self) -> (&'static str, Option<u32>) {
        match self {
            Self::Default => ("default", None),
            Self::Forever => ("forever", None),
            Self::UntilTranscribed => ("until_transcribed", None),
            Self::Days { days } => ("days", Some(days)),
        }
    }

    /// Parse the `(retain_audio, retain_audio_days)` column pair.
    ///
    /// An unknown discriminant falls back to [`RetentionPolicy::Default`]
    /// rather than erroring: a row written by a newer version must not make an
    /// older one refuse to run the sweeper at all, and `default` is the
    /// conservative reading. `days` with a null count is likewise treated as
    /// the default window rather than as "delete immediately", because
    /// guessing wrong in that direction is unrecoverable.
    #[must_use]
    pub fn from_columns(kind: &str, days: Option<u32>) -> Self {
        match kind {
            "forever" => Self::Forever,
            "until_transcribed" => Self::UntilTranscribed,
            "days" => days.map_or(Self::Default, |days| Self::Days { days }),
            _ => Self::Default,
        }
    }

    /// Whether this policy makes the meeting immune to budget eviction.
    #[must_use]
    pub const fn is_forever(self) -> bool {
        matches!(self, Self::Forever)
    }
}

/// The `meetings.state` column (§9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptState {
    /// Still capturing.
    Recording,
    /// Captured, transcript in flight.
    Transcribing,
    /// Transcript complete. This is the event every retention clock starts on.
    Ready,
    /// Transcription failed. The audio is the *only* copy of the meeting, so
    /// no age rule may touch it.
    Failed,
}

impl TranscriptState {
    /// Parse the `state` column, defaulting to the state that protects the
    /// audio if it is unrecognised.
    #[must_use]
    pub fn from_column(s: &str) -> Self {
        match s {
            "ready" => Self::Ready,
            "transcribing" => Self::Transcribing,
            "failed" => Self::Failed,
            _ => Self::Recording,
        }
    }

    /// Whether the transcript exists, i.e. whether the audio is now a backup
    /// rather than the primary artifact.
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// One meeting, as the sweeper sees it.
///
/// Deliberately not a database row: the sweeper's whole value is that it is
/// decidable from this struct alone, with no I/O and no clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingAudio {
    /// The meeting's UUIDv7.
    pub meeting_id: String,
    /// When the meeting started. This is the oldest-first eviction key —
    /// `updated_at` would let a stray edit resurrect a stale recording, and
    /// wall-clock merge order is forbidden anyway (§9.7 invariant 2).
    pub started_at_ms: u64,
    /// Transcription state.
    pub state: TranscriptState,
    /// When the transcript reached `ready`. `None` until it does, which is
    /// what makes every age rule inert before then.
    pub transcript_ready_at_ms: Option<u64>,
    /// This meeting's override.
    pub policy: RetentionPolicy,
    /// Bytes of audio on disk — the Opus tracks and nothing else.
    pub audio_bytes: u64,
    /// Bytes of transcript and derived text. Accounted for so the UI can show
    /// the split, and so a test can prove the sweeper never touches it.
    pub transcript_bytes: u64,
    /// The audio files, relative to the media root. §9.7 invariant 5 forbids
    /// absolute paths in the database; these are resolved against the root at
    /// [`SweepPlan::apply`] time.
    pub audio_paths: Vec<PathBuf>,
}

impl MeetingAudio {
    /// A meeting with no audio and no transcript, for building test cases and
    /// for rows whose media has already been swept.
    #[must_use]
    pub fn new(meeting_id: impl Into<String>, started_at_ms: u64) -> Self {
        Self {
            meeting_id: meeting_id.into(),
            started_at_ms,
            state: TranscriptState::Recording,
            transcript_ready_at_ms: None,
            policy: RetentionPolicy::Default,
            audio_bytes: 0,
            transcript_bytes: 0,
            audio_paths: Vec::new(),
        }
    }

    /// The moment this meeting's audio becomes deletable under its own policy,
    /// or `None` if no age rule applies yet (or ever).
    ///
    /// The clock starts at `transcript_ready_at_ms` for every policy, which is
    /// §9.5's rule generalised: the default is "30 days after ready", so
    /// `days(n)` is the same sentence with `n` substituted, and
    /// `until_transcribed` is `days(0)`. Anchoring `days(n)` to the meeting
    /// start instead would mean a meeting that took two days to transcribe
    /// under a `days(1)` policy was deletable before it was transcribed.
    #[must_use]
    pub fn expires_at_ms(&self, default_days: u32) -> Option<u64> {
        let ready = self.transcript_ready_at_ms?;
        if !self.state.is_ready() {
            // A transcript that regressed out of `ready` (a re-run, a failure)
            // takes the audio's protection back with it.
            return None;
        }
        let days = match self.policy {
            RetentionPolicy::Forever => return None,
            RetentionPolicy::Default => u64::from(default_days),
            RetentionPolicy::UntilTranscribed => 0,
            RetentionPolicy::Days { days } => u64::from(days),
        };
        Some(ready.saturating_add(days.saturating_mul(MS_PER_DAY)))
    }

    /// Whether budget eviction is allowed to consider this meeting.
    ///
    /// `forever` is excluded by §9.5. Un-transcribed audio is excluded because
    /// it is the only copy of the meeting — see the module note on deviations.
    #[must_use]
    pub const fn is_evictable(&self) -> bool {
        !self.policy.is_forever() && self.state.is_ready() && self.audio_bytes > 0
    }
}

/// Global retention configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionSettings {
    /// The window applied to [`RetentionPolicy::Default`] meetings.
    pub default_days: u32,
    /// The global audio ceiling.
    pub budget_bytes: u64,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            default_days: DEFAULT_RETAIN_DAYS,
            budget_bytes: DEFAULT_BUDGET_BYTES,
        }
    }
}

/// Why the sweeper chose to delete a meeting's audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SweepReason {
    /// The retention window elapsed.
    PolicyAge {
        /// The window that elapsed, in days.
        days: u32,
    },
    /// `until_transcribed`, and the transcript arrived.
    Transcribed,
    /// The library was over the global budget and this was the oldest
    /// evictable meeting.
    OverBudget,
}

/// One meeting's audio, scheduled for deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eviction {
    /// Which meeting.
    pub meeting_id: String,
    /// How much this reclaims.
    pub audio_bytes: u64,
    /// Why.
    pub reason: SweepReason,
    /// The audio files to unlink, relative to the media root. Never a
    /// transcript.
    pub paths: Vec<PathBuf>,
}

/// Something the user has to be told rather than silently done to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "warning", rename_all = "snake_case")]
pub enum SweepWarning {
    /// Over budget with nothing left to evict but `forever` meetings.
    ///
    /// §9.5 is explicit that this warns rather than evicting. `forever` means
    /// forever; a budget that silently overrode it would make the setting a
    /// lie, and the user who set it is exactly the user who would not forgive
    /// that.
    OnlyForeverRemains {
        /// How far over the budget the library still is.
        over_by_bytes: u64,
        /// How much of it is held by `forever` meetings.
        forever_bytes: u64,
    },
    /// Over budget with nothing left to evict but meetings that have not been
    /// transcribed yet.
    ///
    /// Not in §9.5; see the module-level note. Deleting these would destroy
    /// the only existing copy of a meeting.
    OnlyUntranscribedRemains {
        /// How far over the budget the library still is.
        over_by_bytes: u64,
        /// How much of it is held by un-transcribed meetings.
        untranscribed_bytes: u64,
    },
}

/// What the sweeper decided, before anything is deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// In the order they should be applied: age-based first, then
    /// oldest-first budget evictions.
    pub evictions: Vec<Eviction>,
    /// Conditions the user must be shown.
    pub warnings: Vec<SweepWarning>,
    /// Audio bytes before the sweep.
    pub audio_bytes_before: u64,
    /// Audio bytes after it.
    pub audio_bytes_after: u64,
    /// The budget this was planned against.
    pub budget_bytes: u64,
}

impl SweepPlan {
    /// How much the plan reclaims.
    #[must_use]
    pub fn bytes_reclaimed(&self) -> u64 {
        self.evictions.iter().map(|e| e.audio_bytes).sum()
    }

    /// Whether the plan leaves the library within budget.
    #[must_use]
    pub const fn within_budget(&self) -> bool {
        self.audio_bytes_after <= self.budget_bytes
    }

    /// Unlink every audio file in the plan, resolving paths against `root`.
    ///
    /// Returns the bytes actually reclaimed and every failure, rather than
    /// stopping at the first one: one unreadable file must not leave the rest
    /// of the library over budget. A file that is already gone counts as
    /// success — the sweeper is idempotent by construction, because the plan
    /// is recomputed from the filesystem each time.
    pub fn apply(&self, root: impl AsRef<Path>) -> (u64, Vec<(PathBuf, std::io::Error)>) {
        let root = root.as_ref();
        let mut reclaimed = 0u64;
        let mut errors = Vec::new();
        for ev in &self.evictions {
            for rel in &ev.paths {
                let path = root.join(rel);
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                match std::fs::remove_file(&path) {
                    Ok(()) => reclaimed += size,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => errors.push((path, e)),
                }
            }
        }
        (reclaimed, errors)
    }
}

/// Decide what to delete. Pure: no clock, no filesystem, no database.
///
/// Two passes, in this order:
///
/// 1. **Policy.** Everything whose retention window has elapsed goes,
///    regardless of the budget. This is the rule the user agreed to.
/// 2. **Budget.** If what survives still exceeds `budget_bytes`, evict
///    oldest-first from the meetings that are eligible — skipping `forever`
///    and skipping anything not yet transcribed — stopping the moment the
///    library is under. If it cannot get under, it warns; it does not reach
///    for the protected sets.
#[must_use]
pub fn plan_sweep(
    now_ms: u64,
    meetings: &[MeetingAudio],
    settings: &RetentionSettings,
) -> SweepPlan {
    let mut plan = SweepPlan {
        budget_bytes: settings.budget_bytes,
        audio_bytes_before: meetings.iter().map(|m| m.audio_bytes).sum(),
        ..SweepPlan::default()
    };

    // Pass 1 — policy.
    let mut expired = Vec::new();
    for m in meetings {
        if m.audio_bytes == 0 && m.audio_paths.is_empty() {
            continue;
        }
        let Some(expires) = m.expires_at_ms(settings.default_days) else {
            continue;
        };
        if now_ms < expires {
            continue;
        }
        let reason = match m.policy {
            RetentionPolicy::UntilTranscribed => SweepReason::Transcribed,
            RetentionPolicy::Days { days } => SweepReason::PolicyAge { days },
            _ => SweepReason::PolicyAge {
                days: settings.default_days,
            },
        };
        expired.push(m.meeting_id.clone());
        plan.evictions.push(Eviction {
            meeting_id: m.meeting_id.clone(),
            audio_bytes: m.audio_bytes,
            reason,
            paths: m.audio_paths.clone(),
        });
    }

    let mut remaining_bytes = plan.audio_bytes_before - plan.bytes_reclaimed();

    // Pass 2 — budget.
    if remaining_bytes > settings.budget_bytes {
        let mut candidates: Vec<&MeetingAudio> = meetings
            .iter()
            .filter(|m| !expired.contains(&m.meeting_id) && m.is_evictable())
            .collect();
        // Oldest first. The tiebreak on id keeps the plan deterministic for
        // two meetings that started in the same millisecond, which matters
        // because this is asserted in tests.
        candidates.sort_by(|a, b| {
            a.started_at_ms
                .cmp(&b.started_at_ms)
                .then_with(|| a.meeting_id.cmp(&b.meeting_id))
        });

        for m in candidates {
            if remaining_bytes <= settings.budget_bytes {
                break;
            }
            remaining_bytes -= m.audio_bytes;
            plan.evictions.push(Eviction {
                meeting_id: m.meeting_id.clone(),
                audio_bytes: m.audio_bytes,
                reason: SweepReason::OverBudget,
                paths: m.audio_paths.clone(),
            });
        }
    }

    // Still over? Say so, naming which protected set is holding the space, so
    // the message can be actionable ("3 meetings marked Keep forever hold
    // 22 GB") rather than a bare "disk full".
    if remaining_bytes > settings.budget_bytes {
        let over_by_bytes = remaining_bytes - settings.budget_bytes;
        let survivors = meetings
            .iter()
            .filter(|m| !expired.contains(&m.meeting_id))
            .filter(|m| !plan.evictions.iter().any(|e| e.meeting_id == m.meeting_id));
        let mut forever_bytes = 0u64;
        let mut untranscribed_bytes = 0u64;
        for m in survivors {
            if m.policy.is_forever() {
                forever_bytes += m.audio_bytes;
            } else if !m.state.is_ready() {
                untranscribed_bytes += m.audio_bytes;
            }
        }
        if forever_bytes > 0 {
            plan.warnings.push(SweepWarning::OnlyForeverRemains {
                over_by_bytes,
                forever_bytes,
            });
        }
        if untranscribed_bytes > 0 {
            plan.warnings.push(SweepWarning::OnlyUntranscribedRemains {
                over_by_bytes,
                untranscribed_bytes,
            });
        }
    }

    plan.audio_bytes_after = remaining_bytes;
    plan
}

/// Disk accounting for the settings screen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    /// Meetings accounted for.
    pub meetings: usize,
    /// Bytes of Opus audio.
    pub audio_bytes: u64,
    /// Bytes of transcript and derived text. Never reclaimable.
    pub transcript_bytes: u64,
    /// Audio held by meetings the sweeper will never touch.
    pub forever_bytes: u64,
    /// Audio held by meetings that have not been transcribed yet.
    pub untranscribed_bytes: u64,
}

impl DiskUsage {
    /// Everything on disk.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.audio_bytes + self.transcript_bytes
    }

    /// How much of the budget is used, 0.0–1.0+ (it can exceed 1.0, which is
    /// precisely the state the warning exists for).
    #[must_use]
    pub fn budget_fraction(&self, budget_bytes: u64) -> f64 {
        if budget_bytes == 0 {
            return 0.0;
        }
        self.audio_bytes as f64 / budget_bytes as f64
    }
}

/// Total up a library.
#[must_use]
pub fn usage(meetings: &[MeetingAudio]) -> DiskUsage {
    let mut u = DiskUsage {
        meetings: meetings.len(),
        ..DiskUsage::default()
    };
    for m in meetings {
        u.audio_bytes += m.audio_bytes;
        u.transcript_bytes += m.transcript_bytes;
        if m.policy.is_forever() {
            u.forever_bytes += m.audio_bytes;
        }
        if !m.state.is_ready() {
            u.untranscribed_bytes += m.audio_bytes;
        }
    }
    u
}

/// Recursive size of a directory, in bytes.
///
/// Used to turn `media/<yyyy>/<mm>/<meeting_id>/` into an
/// [`MeetingAudio::audio_bytes`]. Symlinks are not followed — the media root
/// is ours, and following one out of it would let the number (and, worse, a
/// later `apply`) escape the tree.
pub fn dir_bytes(path: impl AsRef<Path>) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.as_ref().to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}

/// A usage pattern, for sizing the default policy.
///
/// §9.5's table is arithmetic, not measurement, so it belongs in code where it
/// can be checked rather than in prose where it can rot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageProfile {
    /// Meetings recorded on an active day.
    pub meetings_per_day: u32,
    /// Length of one meeting.
    pub minutes_per_meeting: u32,
    /// Days per year on which the user records anything. 250 is a working
    /// year; using 365 here is the mistake that makes the steady-state figure
    /// come out ~45% too high.
    pub active_days_per_year: u32,
    /// Streams kept per meeting. Two: mic and system, never pre-mixed
    /// (CAP-02).
    pub tracks: u32,
    /// Per-track bitrate.
    pub bitrate_bps: u32,
}

/// §9.5's heavy user: five meetings a day, 45 minutes each, 250 days.
pub const HEAVY_USER: UsageProfile = UsageProfile {
    meetings_per_day: 5,
    minutes_per_meeting: 45,
    active_days_per_year: 250,
    tracks: 2,
    bitrate_bps: 24_000,
};

impl UsageProfile {
    /// Bytes one meeting costs.
    #[must_use]
    pub const fn bytes_per_meeting(&self) -> u64 {
        self.minutes_per_meeting as u64 * 60 * self.tracks as u64 * self.bitrate_bps as u64 / 8
    }

    /// Bytes a year of this costs.
    #[must_use]
    pub const fn bytes_per_year(&self) -> u64 {
        self.bytes_per_meeting() * self.meetings_per_day as u64 * self.active_days_per_year as u64
    }

    /// Bytes held at steady state under a `retain_days` window.
    ///
    /// The retention window is in *calendar* days but the recording happens on
    /// *active* days, so the answer is the yearly total scaled by the fraction
    /// of the year the window covers — not `retain_days × daily_rate`, which
    /// over-counts by 365/250.
    #[must_use]
    pub const fn steady_state_bytes(&self, retain_days: u32) -> u64 {
        self.bytes_per_year() * retain_days as u64 / 365
    }
}
