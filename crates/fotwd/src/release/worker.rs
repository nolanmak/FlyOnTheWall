//! The release worker (#326 5.3, D10, D11, D14, D18).
//!
//! Two loops share one library connection:
//!
//! * the **prepare loop** ([`Worker::prepare_pass`], every 60 s) re-checks
//!   the auto unlock and the input fingerprints, creates rows for eligible
//!   meetings and prepares at most [`BATCH`] rows. Model calls run without the
//!   library mutex held;
//! * the **send loop** ([`Worker::send_tick`], every 15 s) runs
//!   [`super::send_one`] for due rows, so a due row sends within 30 s of its
//!   `send_at` unless the worker is paused.
//!
//! Every state change is a compare-and-set ([`super::store::cas`]). Logs carry
//! meeting ids, states and codes only (D15).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fotw_store::Db;
use fotw_web::release::{ReleaseMode, ReleaseSettings};
use rusqlite::types::Value;
use sha2::{Digest as _, Sha256};

use super::settings;
use super::store::{self, NewRelease, Reasons, ReleaseRow, cleared_snapshot, state, text};
use super::types::{
    Cut, Finding, NameCorrection, Payload, PrepareError, PrepareInput, Preparer, ReleaseRecipient,
    ReleaseSummary, SourceSegment,
};
use super::{MailConfig, MailRunner, Preflight, PreflightRequest};
use crate::calendar::CalendarLookup;
pub use crate::summary_guard::PolicyError;

/// How often the prepare loop runs.
pub const PREPARE_INTERVAL: Duration = Duration::from_secs(60);
/// How often the send loop runs.
pub const SEND_INTERVAL: Duration = Duration::from_secs(15);
/// At most this many rows created, prepared or sent per pass.
pub const BATCH: usize = 5;
/// At most this many meetings evaluated for eligibility per pass.
pub const EVALUATE_PER_PASS: usize = 20;
/// A not-eligible reason is re-evaluated at most this often (D10).
pub const ELIGIBILITY_TTL_MS: i64 = 15 * 60 * 1_000;
/// Rows are created at most this long after a meeting ended (D10).
pub const WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
/// A scheduled row this overdue goes to review instead of sending (D10).
pub const DELAYED_MS: i64 = 60 * 60 * 1_000;
/// Each model call's deadline (D18).
pub const CALL_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// `too_long`: more segments than this (D10).
pub const MAX_SEGMENTS: i64 = 4_000;
/// `too_long`: more transcript bytes than this (D10).
pub const MAX_TRANSCRIPT_BYTES: i64 = 200 * 1_000;
/// Auto unlock: at least this many human decisions (D11).
pub const UNLOCK_MIN_DECISIONS: i64 = 20;
/// Auto unlock: at least this share approved unchanged, in percent (D11).
pub const UNLOCK_PERCENT: i64 = 80;
/// `mostly_omitted`: fewer than this share of segments retained, in percent.
pub const MOSTLY_OMITTED_PERCENT: usize = 30;

/// Epoch milliseconds, injected so tests control time.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The calendars to read, and how long to wait for them.
pub type CalendarSource = Arc<dyn Fn() -> (Arc<dyn CalendarLookup>, Duration) + Send + Sync>;

/// The meeting-less policy check run at the start of every pass (D18).
pub trait PolicyCheck: Send + Sync {
    /// Whether the summary policy lets the resolved engine run.
    ///
    /// # Errors
    ///
    /// [`PolicyError`].
    fn check(&self, db: &Db) -> Result<(), PolicyError>;
}

/// The real check: resolve this machine's engine and apply
/// `summary_guard::policy_allows`.
#[derive(Debug, Default)]
pub struct ResolvedEnginePolicy;

impl PolicyCheck for ResolvedEnginePolicy {
    fn check(&self, db: &Db) -> Result<(), PolicyError> {
        let store = crate::secrets::keystore().map_err(|_| PolicyError::Refused)?;
        let engine = crate::engine::resolve_engine(store, db).ok_or(PolicyError::Refused)?;
        crate::summary_guard::policy_allows(db, &engine)
    }
}

/// Worker pause codes (D10). Nothing about a row changes when one is set;
/// each is re-checked every pass and clears on its own.
#[derive(Debug, Default)]
pub struct Pauses {
    settings_unreadable: AtomicBool,
    engine_unavailable: AtomicBool,
    gws_unavailable: AtomicBool,
}

/// One pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// The stored settings are damaged: blocks everything.
    SettingsUnreadable,
    /// The summary policy refuses: blocks preparing.
    EngineUnavailable,
    /// The preflight failed or `gws` would not start: blocks sending.
    GwsUnavailable,
}

impl Pause {
    /// The code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::SettingsUnreadable => "settings_unreadable",
            Self::EngineUnavailable => "engine_unavailable",
            Self::GwsUnavailable => "gws_unavailable",
        }
    }
}

impl Pauses {
    fn flag(&self, pause: Pause) -> &AtomicBool {
        match pause {
            Pause::SettingsUnreadable => &self.settings_unreadable,
            Pause::EngineUnavailable => &self.engine_unavailable,
            Pause::GwsUnavailable => &self.gws_unavailable,
        }
    }

    /// Set or clear `pause`, logging a change.
    pub fn set(&self, pause: Pause, on: bool) {
        if self.flag(pause).swap(on, Ordering::SeqCst) != on {
            if on {
                crate::diag!("  ! release: worker paused ({})", pause.code());
            } else {
                crate::diag!("  release: worker resumed ({})", pause.code());
            }
        }
    }

    /// Whether `pause` is set.
    #[must_use]
    pub fn is(&self, pause: Pause) -> bool {
        self.flag(pause).load(Ordering::SeqCst)
    }

    /// The pause to show, most blocking first.
    #[must_use]
    pub fn current(&self) -> Option<&'static str> {
        [
            Pause::SettingsUnreadable,
            Pause::EngineUnavailable,
            Pause::GwsUnavailable,
        ]
        .into_iter()
        .find(|p| self.is(*p))
        .map(Pause::code)
    }
}

/// What the worker needs.
pub struct Deps {
    /// The library connection the loops and the control share.
    pub db: Arc<Mutex<Db>>,
    /// The data root `release-tmp` lives under.
    pub library: PathBuf,
    /// Cut, summary, render and review (#329, #330).
    pub preparer: Arc<dyn Preparer>,
    /// `gws`.
    pub runner: Arc<dyn MailRunner>,
    /// The calendars.
    pub calendar: CalendarSource,
    /// The pass-start policy check.
    pub policy: Arc<dyn PolicyCheck>,
    /// Now, in epoch ms.
    pub clock: Clock,
}

/// The release worker.
pub struct Worker {
    deps: Deps,
    pauses: Arc<Pauses>,
    preflight: Mutex<Preflight>,
    call_deadline: Duration,
}

/// Lock the shared library, surviving a poisoned mutex: every write is a
/// single committed statement or transaction, so a panic cannot leave a
/// half-applied change behind it.
pub(crate) fn lock(db: &Mutex<Db>) -> MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// SHA-256 over the primary transcript's segments (index, offset, speaker,
/// text) and the name corrections (D14). Notes and the title are not inputs.
#[must_use]
pub fn fingerprint(segments: &[SourceSegment], corrections: &[NameCorrection]) -> String {
    let mut h = Sha256::new();
    let field = |h: &mut Sha256, s: &str| {
        h.update((s.len() as u64).to_le_bytes());
        h.update(s.as_bytes());
    };
    h.update(b"fotw-release-inputs-v1");
    h.update((segments.len() as u64).to_le_bytes());
    for s in segments {
        h.update((s.index as u64).to_le_bytes());
        h.update(s.offset_ms.to_le_bytes());
        field(&mut h, &s.speaker);
        field(&mut h, &s.text);
    }
    h.update((corrections.len() as u64).to_le_bytes());
    for c in corrections {
        field(&mut h, &c.from);
        field(&mut h, &c.to);
    }
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// Everything preparation reads for `row`'s meeting, and its fingerprint.
///
/// # Errors
///
/// The meeting or its transcript could not be read. No content.
pub fn load_input(db: &mut Db, row: &ReleaseRow) -> Result<(PrepareInput, String), &'static str> {
    let meeting = db
        .meetings()
        .get(&row.meeting_id)
        .map_err(|_| "meeting_unreadable")?;
    let stored = crate::summarize::stored_segments(db, &row.meeting_id)
        .map_err(|_| "transcript_unreadable")?;
    let mic_only = stored.iter().all(|s| s.source == fotw_stt::Source::Mic);
    let segments: Vec<SourceSegment> = stored
        .into_iter()
        .enumerate()
        .map(|(index, s)| SourceSegment {
            index,
            offset_ms: s.start_ms,
            speaker: if mic_only {
                "Call audio".to_owned()
            } else {
                s.speaker
                    .unwrap_or_else(|| "Unidentified speaker".to_owned())
            },
            text: s.text,
        })
        .collect();
    let corrections: Vec<NameCorrection> =
        crate::documents::name_corrections_for(db, &row.meeting_id)
            .into_iter()
            .map(|c| NameCorrection {
                from: c.from,
                to: c.to,
            })
            .collect();
    let print = fingerprint(&segments, &corrections);
    Ok((
        PrepareInput {
            meeting_id: row.meeting_id.clone(),
            event_title: row.event_title.clone(),
            tz: meeting.tz,
            started_at_ms: meeting.started_at_ms,
            segments,
            corrections,
            include_summary_spellings: matches!(
                crate::invitees::enabled(db),
                Ok(None | Some(true))
            ),
            recipients: row
                .recipients
                .iter()
                .map(|r| ReleaseRecipient {
                    address: r.address.clone(),
                    display_name: r.display_name.clone(),
                    is_self: r.is_self,
                })
                .collect(),
        },
        print,
    ))
}

/// Whether auto mode is unlocked for `engine_id` (D11): at least 20 human
/// decisions, at least 80% of them `approved_unchanged`.
#[must_use]
pub fn auto_unlocked(conn: &rusqlite::Connection, engine_id: &str) -> bool {
    match store::decision_counts(conn, engine_id) {
        Ok((human, unchanged)) => {
            human >= UNLOCK_MIN_DECISIONS && unchanged * 100 >= human * UNLOCK_PERCENT
        }
        Err(_) => false,
    }
}

/// The auto unlock, re-checked every pass (D11): when auto is selected but
/// locked, every `scheduled` row approved by auto goes back to review.
pub(crate) fn enforce_auto_lock(
    conn: &rusqlite::Connection,
    settings: &ReleaseSettings,
    engine_id: &str,
    now_ms: i64,
) {
    if settings.mode != ReleaseMode::Auto || auto_unlocked(conn, engine_id) {
        return;
    }
    return_auto_rows(conn, "mode_changed", now_ms);
}

/// Every `scheduled` row approved by auto → `needs_review(reason)`.
pub(crate) fn return_auto_rows(conn: &rusqlite::Connection, reason: &str, now_ms: i64) {
    for row in store::in_states(conn, &[state::SCHEDULED]).unwrap_or_default() {
        if row.approved_by.as_deref() == Some("auto") {
            to_review(conn, &row.meeting_id, state::SCHEDULED, &[reason], now_ms);
        }
    }
}

/// Every `scheduled` row → `needs_review(reason)`.
pub(crate) fn return_all_scheduled(conn: &rusqlite::Connection, reason: &str, now_ms: i64) {
    for row in store::in_states(conn, &[state::SCHEDULED]).unwrap_or_default() {
        to_review(conn, &row.meeting_id, state::SCHEDULED, &[reason], now_ms);
    }
}

fn to_review(conn: &rusqlite::Connection, id: &str, from: &str, reasons: &[&str], now_ms: i64) {
    match store::cas(
        conn,
        id,
        from,
        state::NEEDS_REVIEW,
        Reasons::Set(reasons),
        &[("send_at", Value::Null), ("approved_by", Value::Null)],
        now_ms,
    ) {
        Ok(true) => crate::diag!(
            "  release: meeting {id} -> needs_review ({})",
            reasons.join(",")
        ),
        Ok(false) => {}
        Err(_) => crate::diag!("  ! release: meeting {id} not moved (store_failed)"),
    }
}

/// A fingerprint mismatch (D14): back to `preparing` from the cut, ending in
/// `needs_review(inputs_changed)`. `send_outcome_unknown` rows stay put.
pub(crate) fn restart_for_inputs(
    conn: &rusqlite::Connection,
    row: &ReleaseRow,
    now_ms: i64,
) -> bool {
    if row.has("send_outcome_unknown") {
        return false;
    }
    let mut sets = cleared_snapshot();
    sets.push(("input_fingerprint", Value::Null));
    let moved = store::cas(
        conn,
        &row.meeting_id,
        &row.state,
        state::PREPARING,
        Reasons::Set(&["inputs_changed"]),
        &sets,
        now_ms,
    )
    .unwrap_or(false);
    if moved {
        crate::diag!(
            "  release: meeting {} -> preparing (inputs_changed)",
            row.meeting_id
        );
    }
    moved
}

/// A finished preparation.
struct Prepared {
    cut: Cut,
    summary: ReleaseSummary,
    payload: Payload,
    findings: Vec<Finding>,
}

/// A meeting the eligibility check reads, before the calendar.
struct Candidate {
    id: String,
    state: String,
    started_at_ms: i64,
    ended_at_ms: Option<i64>,
    updated_at: i64,
    calendar_uid: Option<String>,
    calendar_source: Option<String>,
}

impl Worker {
    /// A worker over `deps`.
    #[must_use]
    pub fn new(deps: Deps) -> Self {
        Self {
            deps,
            pauses: Arc::new(Pauses::default()),
            preflight: Mutex::new(Preflight::default()),
            call_deadline: CALL_DEADLINE,
        }
    }

    /// The same worker with a shorter model-call deadline. For tests.
    #[must_use]
    pub fn with_call_deadline(mut self, deadline: Duration) -> Self {
        self.call_deadline = deadline;
        self
    }

    /// The pause flags, shared with the control.
    #[must_use]
    pub fn pauses(&self) -> Arc<Pauses> {
        Arc::clone(&self.pauses)
    }

    fn db(&self) -> MutexGuard<'_, Db> {
        lock(&self.deps.db)
    }

    /// The settings in force, or `None` (and `settings_unreadable`) when the
    /// stored value is damaged.
    fn settings(&self) -> Option<ReleaseSettings> {
        let loaded = settings::load(&self.db()).in_force();
        self.pauses.set(Pause::SettingsUnreadable, loaded.is_none());
        loaded
    }

    /// At daemon start: a row left in `sending` may or may not have gone out,
    /// so it needs a person (D13). Returns how many rows moved.
    pub fn recover(&self) -> usize {
        let now = (self.deps.clock)();
        let db = self.db();
        let rows = store::in_states(db.conn(), &[state::SENDING]).unwrap_or_default();
        let mut moved = 0;
        for row in rows {
            if store::cas(
                db.conn(),
                &row.meeting_id,
                state::SENDING,
                state::NEEDS_REVIEW,
                Reasons::Set(&["send_outcome_unknown"]),
                &[],
                now,
            )
            .unwrap_or(false)
            {
                crate::diag!(
                    "  release: meeting {} -> needs_review (send_outcome_unknown)",
                    row.meeting_id
                );
                moved += 1;
            }
        }
        moved
    }

    // ------------------------------------------------------------ prepare

    /// One prepare-loop pass.
    pub async fn prepare_pass(&self) {
        let now = (self.deps.clock)();
        let Some(settings) = self.settings() else {
            return;
        };
        if !settings.enabled {
            return;
        }
        let engine_id = self.deps.preparer.engine_id();
        {
            let db = self.db();
            enforce_auto_lock(db.conn(), &settings, &engine_id, now);
            match self.deps.policy.check(&db) {
                Ok(()) => self.pauses.set(Pause::EngineUnavailable, false),
                Err(PolicyError::Refused) => {
                    self.pauses.set(Pause::EngineUnavailable, true);
                    return;
                }
                // Settings moved under the check: try again next pass.
                Err(PolicyError::SettingsChanged | PolicyError::InputsChanged) => return,
            }
        }
        self.check_fingerprints(now);
        self.create_rows(&settings, now);
        let rows = {
            let db = self.db();
            store::in_states(db.conn(), &[state::PREPARING]).unwrap_or_default()
        };
        for row in rows.into_iter().take(BATCH) {
            if self.pauses.is(Pause::EngineUnavailable) {
                break;
            }
            self.prepare_one(row, &engine_id).await;
        }
    }

    /// D14: rows in `needs_review`, `scheduled` or `held` whose inputs changed
    /// go back to `preparing`.
    fn check_fingerprints(&self, now: i64) {
        let mut db = self.db();
        let rows = store::in_states(
            db.conn(),
            &[state::NEEDS_REVIEW, state::SCHEDULED, state::HELD],
        )
        .unwrap_or_default();
        for row in rows {
            let Some(stored) = row.input_fingerprint.clone() else {
                continue;
            };
            if row.has("send_outcome_unknown") {
                continue;
            }
            let current = load_input(&mut db, &row).map(|(_, print)| print);
            if current.as_deref() != Ok(stored.as_str()) {
                restart_for_inputs(db.conn(), &row, now);
            }
        }
    }

    /// Evaluate eligibility and create at most [`BATCH`] rows.
    fn create_rows(&self, settings: &ReleaseSettings, now: i64) {
        let candidates = {
            let db = self.db();
            match candidates(db.conn(), settings.release_since_ms, now) {
                Ok(c) => c,
                Err(_) => {
                    crate::diag!("  ! release: eligibility read failed (store_failed)");
                    return;
                }
            }
        };
        let mut created = 0;
        for c in candidates {
            if created >= BATCH {
                break;
            }
            match self.evaluate(&c, settings, now) {
                Ok((event, recipients)) => {
                    let db = self.db();
                    let new = NewRelease {
                        meeting_id: &c.id,
                        source: event.source,
                        source_event_id: &event.uid,
                        ical_uid: event.ical_uid.as_deref(),
                        event_title: &event.title,
                        recipients: &recipients,
                    };
                    if store::insert_preparing(db.conn(), &new, now).unwrap_or(false) {
                        let _ = db.conn().execute(
                            "DELETE FROM release_eligibility WHERE meeting_id = ?1",
                            [&c.id],
                        );
                        crate::diag!("  release: meeting {} -> preparing", c.id);
                        created += 1;
                    }
                }
                Err(reason) => {
                    let db = self.db();
                    let _ = store::put_eligibility(db.conn(), &c.id, reason, now);
                }
            }
        }
    }

    /// Whether `c` is eligible (5.3), and its event and recipients if so.
    fn evaluate(
        &self,
        c: &Candidate,
        settings: &ReleaseSettings,
        now: i64,
    ) -> Result<
        (
            crate::calendar::ScheduledEvent,
            Vec<crate::calendar::release::Recipient>,
        ),
        &'static str,
    > {
        {
            let mut db = self.db();
            if c.state != "ready" {
                return Err("not_ready");
            }
            let receipts = crate::enrich::read_receipts(&db);
            if !crate::github::export_ready(
                receipts.get(&c.id),
                c.updated_at,
                u64::try_from(now).unwrap_or(0),
            ) {
                return Err("not_ready");
            }
            let Some(transcript) = crate::persist::primary_transcript_id(&mut db, &c.id) else {
                return Err("not_ready");
            };
            let (count, bytes): (i64, i64) = db
                .conn()
                .query_row(
                    "SELECT count(*), COALESCE(SUM(length(CAST(text AS BLOB))), 0)
                       FROM segments WHERE transcript_id = ?1 AND meeting_id = ?2",
                    [&transcript, &c.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|_| "not_ready")?;
            if count == 0 {
                return Err("not_ready");
            }
            if count > MAX_SEGMENTS || bytes > MAX_TRANSCRIPT_BYTES {
                return Err("too_long");
            }
        }
        let (Some(uid), Some(source)) = (
            c.calendar_uid.as_deref().filter(|u| !u.is_empty()),
            c.calendar_source.as_deref().filter(|s| !s.is_empty()),
        ) else {
            return Err("no_confident_match");
        };
        // The calendar is read without the library mutex held.
        let (calendar, timeout) = (self.deps.calendar)();
        let event = crate::calendar::release::lookup_confident(
            calendar,
            source,
            uid,
            c.started_at_ms,
            c.ended_at_ms.unwrap_or(c.started_at_ms),
            timeout,
        )
        .map_err(crate::calendar::release::NotEligible::code)?;
        let recipients = crate::calendar::release::recipients(&event, &settings.allowed_domains);
        if recipients.list.is_empty() {
            return Err("no_recipients");
        }
        Ok((event, recipients.list))
    }

    /// Prepare one `preparing` row.
    async fn prepare_one(&self, row: ReleaseRow, engine_id: &str) {
        let loaded = {
            let mut db = self.db();
            load_input(&mut db, &row)
        };
        let id = row.meeting_id.clone();
        let Ok((input, print)) = loaded else {
            self.hold(&id, "model_failed", None, None, engine_id);
            return;
        };
        let reviewer_cut = row.cut.clone().filter(|c| c.reviewer_retained.is_some());
        match self.run_steps(&input, reviewer_cut).await {
            Ok(prepared) => self.commit(&row, &input, &print, prepared, engine_id),
            Err((PrepareError::PolicyRefused, _)) => {
                self.pauses.set(Pause::EngineUnavailable, true);
            }
            // D18: retried next pass; preparation restarts with fresh inputs.
            Err((PrepareError::SettingsChanged | PrepareError::InputsChanged, _)) => {}
            Err((PrepareError::ModelFailed, cut)) => {
                self.hold(&id, "model_failed", cut.as_ref(), Some(&print), engine_id);
            }
            Err((PrepareError::BodyTooLarge, cut)) => {
                self.hold(&id, "body_too_large", cut.as_ref(), Some(&print), engine_id);
            }
        }
    }

    async fn call<T>(
        &self,
        work: impl std::future::Future<Output = Result<T, PrepareError>>,
    ) -> Result<T, PrepareError> {
        match tokio::time::timeout(self.call_deadline, work).await {
            Ok(result) => result,
            Err(_) => Err(PrepareError::ModelFailed),
        }
    }

    async fn run_steps(
        &self,
        input: &PrepareInput,
        reviewer_cut: Option<Cut>,
    ) -> Result<Prepared, (PrepareError, Option<Cut>)> {
        let preparer = &self.deps.preparer;
        let cut = match reviewer_cut {
            // Edit cuts: from the summary step (5.3).
            Some(cut) => cut,
            None => self
                .call(preparer.cut(input))
                .await
                .map_err(|e| (e, None))?,
        };
        let retained = cut.retained(input.segments.len());
        let fail = |e: PrepareError| (e, Some(cut.clone()));
        if retained.is_empty() {
            return Err(fail(PrepareError::ModelFailed));
        }
        let summary = self
            .call(preparer.summarize(input, &retained))
            .await
            .map_err(fail)?;
        let payload = preparer.render(input, &retained, &summary).map_err(fail)?;
        let findings = self
            .call(preparer.review(input, &retained, &payload))
            .await
            .map_err(fail)?;
        Ok(Prepared {
            cut,
            summary,
            payload,
            findings,
        })
    }

    fn hold(
        &self,
        id: &str,
        reason: &str,
        cut: Option<&Cut>,
        print: Option<&str>,
        engine_id: &str,
    ) {
        let now = (self.deps.clock)();
        let mut sets = cut.map(cut_columns).unwrap_or_default();
        sets.push(("input_fingerprint", text(print.map(str::to_owned))));
        sets.push(("engine_id", Value::Text(engine_id.to_owned())));
        let db = self.db();
        if store::cas(
            db.conn(),
            id,
            state::PREPARING,
            state::HELD,
            Reasons::Set(&[reason]),
            &sets,
            now,
        )
        .unwrap_or(false)
        {
            crate::diag!("  release: meeting {id} -> held ({reason})");
        }
    }

    /// Leave `preparing` per D11.
    fn commit(
        &self,
        row: &ReleaseRow,
        input: &PrepareInput,
        print: &str,
        prepared: Prepared,
        engine_id: &str,
    ) {
        let now = (self.deps.clock)();
        let mut db = self.db();
        // D14/D18: inputs that moved during the calls restart preparation.
        match load_input(&mut db, row) {
            Ok((_, current)) if current == print => {}
            _ => return,
        }
        // The mode is read when a row leaves `preparing` (D11).
        let Some(settings) = settings::load(&db).in_force() else {
            return;
        };
        let conn = db.conn();
        let total = input.segments.len();
        let retained = prepared.cut.retained(total);
        let mut reasons: Vec<&str> = row
            .reasons
            .iter()
            .map(String::as_str)
            .filter(|r| matches!(*r, "edited" | "inputs_changed"))
            .collect();
        if !prepared.findings.is_empty() {
            reasons.push("has_findings");
        }
        if row.recipients.len() > crate::calendar::release::MAX_RECIPIENTS {
            reasons.push("many_recipients");
        }
        if retained.len() * 100 < total * MOSTLY_OMITTED_PERCENT {
            reasons.push("mostly_omitted");
        }
        let addresses: Vec<String> = row.recipients.iter().map(|r| r.address.clone()).collect();
        if !store::all_known(conn, &addresses).unwrap_or(false) {
            reasons.push("first_time_recipient");
        }
        match settings.mode {
            ReleaseMode::Approval => reasons.push("review_required"),
            ReleaseMode::Auto if !auto_unlocked(conn, engine_id) => reasons.push("auto_locked"),
            ReleaseMode::Auto => {}
        }

        let mut sets = cut_columns(&prepared.cut);
        sets.extend([
            (
                "summary_json",
                Value::Text(store::summary_to_json(&prepared.summary)),
            ),
            (
                "rendered_subject",
                Value::Text(prepared.payload.subject.clone()),
            ),
            ("rendered_body", Value::Text(prepared.payload.body.clone())),
            (
                "rendered_attachment",
                Value::Text(prepared.payload.attachment.clone()),
            ),
            (
                "rendered_attachment_name",
                Value::Text(prepared.payload.attachment_name.clone()),
            ),
            (
                "index_map_json",
                Value::Text(store::index_map_to_json(&prepared.payload.index_map)),
            ),
            (
                "findings_json",
                Value::Text(store::findings_to_json(&prepared.findings)),
            ),
            ("input_fingerprint", Value::Text(print.to_owned())),
            ("engine_id", Value::Text(engine_id.to_owned())),
        ]);
        let id = &row.meeting_id;
        let result = (|| -> rusqlite::Result<bool> {
            let tx = conn.unchecked_transaction()?;
            let applied = if reasons.is_empty() {
                let send_at = now + i64::from(settings.send_delay_minutes) * 60_000;
                sets.push(("approved_by", Value::Text("auto".to_owned())));
                sets.push(("send_at", Value::Integer(send_at)));
                let applied = store::cas_in(
                    &tx,
                    id,
                    state::PREPARING,
                    state::SCHEDULED,
                    Reasons::Set(&[]),
                    &sets,
                    now,
                )?;
                if applied {
                    store::record_decision(&tx, id, engine_id, store::decision::AUTO, now)?;
                }
                applied
            } else {
                sets.push(("approved_by", Value::Null));
                sets.push(("send_at", Value::Null));
                store::cas_in(
                    &tx,
                    id,
                    state::PREPARING,
                    state::NEEDS_REVIEW,
                    Reasons::Set(&reasons),
                    &sets,
                    now,
                )?
            };
            tx.commit()?;
            Ok(applied)
        })();
        match result {
            Ok(true) if reasons.is_empty() => {
                crate::diag!("  release: meeting {id} -> scheduled (auto)");
            }
            Ok(true) => crate::diag!(
                "  release: meeting {id} -> needs_review ({})",
                store::canonical(&reasons).join(",")
            ),
            Ok(false) => {}
            Err(_) => crate::diag!("  ! release: meeting {id} not stored (store_failed)"),
        }
    }

    // ------------------------------------------------------------ send

    /// One send-loop tick.
    pub fn send_tick(&self) {
        let now = (self.deps.clock)();
        let Some(settings) = self.settings() else {
            return;
        };
        if !settings.enabled {
            return;
        }
        let engine_id = self.deps.preparer.engine_id();
        let due = {
            let db = self.db();
            enforce_auto_lock(db.conn(), &settings, &engine_id, now);
            // D10: a row overdue by more than an hour (the worker was paused
            // or the daemon was down) goes to a person instead of sending.
            for row in store::in_states(db.conn(), &[state::SCHEDULED]).unwrap_or_default() {
                if row.send_at.is_some_and(|at| now - at > DELAYED_MS) {
                    to_review(
                        db.conn(),
                        &row.meeting_id,
                        state::SCHEDULED,
                        &["delayed_while_paused"],
                        now,
                    );
                }
            }
            store::due(db.conn(), now, BATCH).unwrap_or_default()
        };
        let notify = settings.notify_address.trim().to_owned();
        if due.is_empty() && notify.is_empty() && !self.pauses.is(Pause::GwsUnavailable) {
            return;
        }
        if settings.verified_sender.is_empty() {
            self.pauses.set(Pause::GwsUnavailable, true);
            return;
        }
        let config = MailConfig {
            profile_dir: PathBuf::from(&settings.gws_profile_dir),
            binary: Some(settings.gws_binary.trim())
                .filter(|b| !b.is_empty())
                .map(PathBuf::from),
        };
        let request = PreflightRequest {
            config: &config,
            allowed_domains: &settings.allowed_domains,
            verified_sender: Some(&settings.verified_sender),
        };
        let pass = {
            let mut preflight = self
                .preflight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            preflight.check(&*self.deps.runner, &request, now)
        };
        let pass = match pass {
            Ok(pass) => {
                self.pauses.set(Pause::GwsUnavailable, false);
                pass
            }
            Err(e) => {
                crate::diag!("  ! release: preflight failed ({})", e.code());
                self.pauses.set(Pause::GwsUnavailable, true);
                return;
            }
        };
        let store = store::DbStore::new(Arc::clone(&self.deps.db), Arc::clone(&self.deps.clock));
        for row in due {
            let Some(send_row) = row.send_row() else {
                continue;
            };
            match super::send_one(
                &*self.deps.runner,
                &store,
                &pass,
                &self.deps.library,
                &settings.allowed_domains,
                &send_row,
            ) {
                Ok(outcome) if outcome.pauses_worker() => {
                    self.pauses.set(Pause::GwsUnavailable, true);
                    break;
                }
                Ok(_) => {}
                Err(e) => crate::diag!(
                    "  ! release: meeting {} not sent ({})",
                    row.meeting_id,
                    e.code()
                ),
            }
        }
        if !notify.is_empty() {
            let _ = super::send_notice(
                &*self.deps.runner,
                &store,
                &pass,
                &settings.allowed_domains,
                Some(&notify),
                (self.deps.clock)(),
            );
        }
    }
}

/// The cut columns of a row.
fn cut_columns(cut: &Cut) -> Vec<(&'static str, Value)> {
    vec![
        (
            "end_index",
            Value::Integer(i64::try_from(cut.end_index).unwrap_or(i64::MAX)),
        ),
        (
            "omit_ranges_json",
            Value::Text(store::json(&cut.omit_ranges)),
        ),
        (
            "reviewer_retained_json",
            cut.reviewer_retained
                .as_ref()
                .map_or(Value::Null, |r| Value::Text(store::json(r))),
        ),
    ]
}

/// Meetings to evaluate: after the cutoff, inside the window, without a row,
/// and not evaluated in the last 15 minutes. Oldest first.
fn candidates(
    conn: &rusqlite::Connection,
    since_ms: i64,
    now_ms: i64,
) -> rusqlite::Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.state, m.started_at_ms, m.ended_at_ms, m.updated_at,
                m.calendar_uid, m.calendar_source
           FROM meetings m
          WHERE m.started_at_ms >= ?1
            AND COALESCE(m.ended_at_ms, m.started_at_ms) >= ?2
            AND NOT EXISTS (SELECT 1 FROM meeting_releases r WHERE r.meeting_id = m.id)
            AND NOT EXISTS (SELECT 1 FROM release_eligibility e
                             WHERE e.meeting_id = m.id AND e.evaluated_at > ?3)
          ORDER BY m.started_at_ms, m.id
          LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            since_ms,
            now_ms - WINDOW_MS,
            now_ms - ELIGIBILITY_TTL_MS,
            i64::try_from(EVALUATE_PER_PASS).unwrap_or(20)
        ],
        |r| {
            Ok(Candidate {
                id: r.get(0)?,
                state: r.get(1)?,
                started_at_ms: r.get(2)?,
                ended_at_ms: r.get(3)?,
                updated_at: r.get(4)?,
                calendar_uid: r.get(5)?,
                calendar_source: r.get(6)?,
            })
        },
    )?;
    rows.collect()
}
