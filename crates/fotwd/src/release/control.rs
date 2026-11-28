//! [`ReleaseControl`] for the web layer (#326 5.1, 5.7): settings, the
//! meeting line, the Outbox and its actions. Every action is a
//! compare-and-set; one that loses returns a reload message and changes
//! nothing. Error strings are user-facing and carry no content (D15).

use std::sync::{Arc, Mutex};

use fotw_store::Db;
use fotw_web::release::{
    BoxFuture, MeetingReleaseStatus, Outbox, OutboxAction, OutboxDetail, OutboxFinding, OutboxItem,
    OutboxRecipient, OutboxSegment, ReleaseControl, ReleaseMode, ReleaseSettings,
    ReleaseSettingsResponse,
};
use rusqlite::types::Value;

use super::settings::{self, Stored};
use super::store::{self, Reasons, ReleaseRow, cleared_snapshot, decision, section_name, state};
use super::types::Preparer;
use super::worker::{
    Clock, Pause, Pauses, PolicyCheck, WINDOW_MS, auto_unlocked, load_input, lock,
    restart_for_inputs, return_all_scheduled, return_auto_rows,
};
use super::{MailConfig, MailRunner, Preflight, PreflightRequest};

const MOVED: &str = "This release changed state. Reload the Outbox.";
const NO_ROW: &str = "This meeting has no release.";
const STORE_FAILED: &str = "The release could not be updated. Try again.";
const UNREADABLE: &str =
    "The stored release settings could not be read. Save a complete replacement to continue.";

/// The daemon's [`ReleaseControl`].
pub struct Control {
    inner: Arc<Inner>,
}

struct Inner {
    db: Arc<Mutex<Db>>,
    runner: Arc<dyn MailRunner>,
    policy: Arc<dyn PolicyCheck>,
    preparer: Arc<dyn Preparer>,
    clock: Clock,
    pauses: Arc<Pauses>,
    preflight: Mutex<Preflight>,
}

impl Control {
    /// A control over the worker's connection and pause flags.
    #[must_use]
    pub fn new(
        db: Arc<Mutex<Db>>,
        runner: Arc<dyn MailRunner>,
        policy: Arc<dyn PolicyCheck>,
        preparer: Arc<dyn Preparer>,
        clock: Clock,
        pauses: Arc<Pauses>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                runner,
                policy,
                preparer,
                clock,
                pauses,
                preflight: Mutex::new(Preflight::default()),
            }),
        }
    }

    /// [`ReleaseControl::set_settings`] without the future, for tests and
    /// blocking callers.
    #[must_use]
    pub fn save_settings(&self, incoming: ReleaseSettings) -> ReleaseSettingsResponse {
        self.inner.save(incoming)
    }
}

impl Inner {
    fn response(&self) -> ReleaseSettingsResponse {
        let db = lock(&self.db);
        let stored = settings::load(&db);
        let unlocked = auto_unlocked(db.conn(), &self.preparer.engine_id());
        let damaged = stored == Stored::Damaged;
        ReleaseSettingsResponse {
            settings: stored.in_force().unwrap_or_default(),
            error: damaged.then(|| UNREADABLE.to_owned()),
            auto_unlocked: unlocked,
            paused: if damaged {
                Some(Pause::SettingsUnreadable.code().to_owned())
            } else {
                self.pauses.current().map(str::to_owned)
            },
        }
    }

    fn refuse(&self, message: &str) -> ReleaseSettingsResponse {
        let mut response = self.response();
        response.error = Some(message.to_owned());
        response
    }

    fn save(&self, incoming: ReleaseSettings) -> ReleaseSettingsResponse {
        let now = (self.clock)();
        let engine_id = self.preparer.engine_id();
        let (previous, unlocked) = {
            let db = lock(&self.db);
            let previous = match settings::load(&db) {
                Stored::Valid(s) => Some(s),
                // A damaged value counts as "release off": saving a complete
                // replacement is how it is repaired, and enabling from it
                // re-stamps the cutoff and re-runs every check.
                Stored::Missing | Stored::Damaged => None,
            };
            (previous, auto_unlocked(db.conn(), &engine_id))
        };
        let mut next = settings::stamp(previous.as_ref(), incoming, now);
        if let Err(message) = settings::validate(&next) {
            return self.refuse(message);
        }
        if next.mode == ReleaseMode::Auto && !unlocked {
            return self.refuse(
                "Auto mode unlocks after 20 reviewed releases with this engine, at least 80% approved unchanged.",
            );
        }
        let was_enabled = previous.as_ref().is_some_and(|p| p.enabled);
        let gws_changed = previous.as_ref().is_none_or(|p| {
            p.gws_profile_dir != next.gws_profile_dir
                || p.gws_binary != next.gws_binary
                || p.allowed_domains != next.allowed_domains
        });
        if next.enabled {
            if !was_enabled {
                let allowed = self.policy.check(&lock(&self.db));
                if allowed.is_err() {
                    return self.refuse(
                        "The summary policy does not allow release. Choose a summary engine first.",
                    );
                }
            }
            if gws_changed || !was_enabled || next.verified_sender.is_empty() {
                let config = MailConfig {
                    profile_dir: next.gws_profile_dir.clone().into(),
                    binary: Some(next.gws_binary.trim())
                        .filter(|b| !b.is_empty())
                        .map(Into::into),
                };
                let request = PreflightRequest {
                    config: &config,
                    allowed_domains: &next.allowed_domains,
                    verified_sender: None,
                };
                let checked = self
                    .preflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .check_fresh(&*self.runner, &request, now);
                match checked {
                    Ok(pass) => {
                        next.verified_sender = pass.sender().to_owned();
                        self.pauses.set(Pause::GwsUnavailable, false);
                    }
                    Err(e) => {
                        return self.refuse(&format!(
                            "The Google Workspace CLI check failed ({}).",
                            e.code()
                        ));
                    }
                }
            }
        } else if gws_changed {
            next.verified_sender.clear();
        }

        {
            let db = lock(&self.db);
            if settings::write(&db, &next, now).is_err() {
                return self.refuse("The release settings could not be saved. Try again.");
            }
            // 5.1 effects on non-terminal rows.
            if next.enabled && !was_enabled {
                return_all_scheduled(db.conn(), "reenabled_while_scheduled", now);
            }
            if previous
                .as_ref()
                .is_some_and(|p| p.mode == ReleaseMode::Auto)
                && next.mode == ReleaseMode::Approval
            {
                return_auto_rows(db.conn(), "mode_changed", now);
            }
        }
        self.pauses.set(Pause::SettingsUnreadable, false);
        crate::diag!(
            "  release: settings saved (enabled={}, mode={})",
            next.enabled,
            if next.mode == ReleaseMode::Auto {
                "auto"
            } else {
                "approval"
            }
        );
        self.response()
    }

    fn status(&self, meeting_id: &str) -> MeetingReleaseStatus {
        let now = (self.clock)();
        let mut db = lock(&self.db);
        if let Ok(Some(row)) = store::get(db.conn(), meeting_id) {
            let recipient_count = row.sent_to.as_ref().map_or(row.recipients.len(), Vec::len);
            return MeetingReleaseStatus {
                state: row.state,
                reasons: row.reasons,
                recipient_count,
                send_at: row.send_at,
                sent_at: row.sent_at,
                event_title: Some(row.event_title),
                event_start_ms: row.event_start_ms,
                event_end_ms: row.event_end_ms,
            };
        }
        let not_eligible = |reason: &str| MeetingReleaseStatus {
            state: "not_eligible".to_owned(),
            reasons: vec![reason.to_owned()],
            recipient_count: 0,
            send_at: None,
            sent_at: None,
            event_title: None,
            event_start_ms: None,
            event_end_ms: None,
        };
        let Some(current) = settings::load(&db).in_force().filter(|s| s.enabled) else {
            return not_eligible("release_off");
        };
        let Ok(meeting) = db.meetings().get(meeting_id) else {
            return not_eligible("not_ready");
        };
        if meeting.started_at_ms < current.release_since_ms {
            return not_eligible("before_cutoff");
        }
        let ended = meeting.ended_at_ms.unwrap_or(meeting.started_at_ms);
        if now - ended > WINDOW_MS {
            return not_eligible("window_passed");
        }
        match store::eligibility(db.conn(), meeting_id) {
            Ok(Some((reason, _))) => not_eligible(&reason),
            _ => not_eligible("not_ready"),
        }
    }

    fn outbox(&self) -> Outbox {
        let db = lock(&self.db);
        let rows = store::in_states(
            db.conn(),
            &[state::NEEDS_REVIEW, state::SCHEDULED, state::HELD],
        )
        .unwrap_or_default();
        let damaged = settings::load(&db) == Stored::Damaged;
        Outbox {
            badge: rows
                .iter()
                .filter(|r| r.state == state::NEEDS_REVIEW || r.state == state::HELD)
                .count(),
            items: rows.iter().map(item).collect(),
            paused: if damaged {
                Some(Pause::SettingsUnreadable.code().to_owned())
            } else {
                self.pauses.current().map(str::to_owned)
            },
        }
    }

    fn detail(&self, meeting_id: &str) -> Option<OutboxDetail> {
        let mut db = lock(&self.db);
        let row = store::get(db.conn(), meeting_id).ok().flatten()?;
        Some(build_detail(&mut db, &row))
    }

    /// Whether the row's inputs changed since it was prepared (D14).
    fn inputs_changed(db: &mut Db, row: &ReleaseRow) -> bool {
        match (&row.input_fingerprint, load_input(db, row)) {
            (Some(stored), Ok((_, current))) => *stored != current,
            (Some(_), Err(_)) => true,
            (None, _) => false,
        }
    }

    fn act(&self, meeting_id: &str, action: OutboxAction) -> Result<OutboxDetail, String> {
        let now = (self.clock)();
        let current_engine = self.preparer.engine_id();
        let mut db = lock(&self.db);
        let row = store::get(db.conn(), meeting_id)
            .map_err(|_| STORE_FAILED.to_owned())?
            .ok_or_else(|| NO_ROW.to_owned())?;
        let engine_id = row.engine_id.clone().unwrap_or(current_engine);
        let id = &row.meeting_id;
        let unknown = row.has("send_outcome_unknown");
        let moved = || MOVED.to_owned();
        let applied = match action {
            OutboxAction::Approve => {
                if row.state != state::NEEDS_REVIEW || unknown {
                    return Err(moved());
                }
                if Self::inputs_changed(&mut db, &row) {
                    restart_for_inputs(db.conn(), &row, now);
                    return Err(
                        "The meeting changed since this release was prepared. It is being prepared again."
                            .to_owned(),
                    );
                }
                let edited = row.has("edited")
                    || row
                        .cut
                        .as_ref()
                        .is_some_and(|c| c.reviewer_retained.is_some());
                let choice = if edited {
                    decision::APPROVED_EDITED
                } else {
                    decision::APPROVED_UNCHANGED
                };
                schedule_now(db.conn(), &row, now, Some((engine_id.as_str(), choice)))
            }
            OutboxAction::SendAgain => {
                if row.state != state::NEEDS_REVIEW || !unknown {
                    return Err(moved());
                }
                schedule_now(
                    db.conn(),
                    &row,
                    now,
                    Some((engine_id.as_str(), decision::SENT_AGAIN)),
                )
            }
            OutboxAction::EditCuts { retained } => {
                let editable = (row.state == state::NEEDS_REVIEW && !unknown)
                    || (row.state == state::HELD && row.has("body_too_large"));
                if !editable {
                    return Err(moved());
                }
                let len = load_input(&mut db, &row)
                    .map(|(input, _)| input.segments.len())
                    .map_err(|_| STORE_FAILED.to_owned())?;
                let mut chosen = retained;
                chosen.sort_unstable();
                chosen.dedup();
                if chosen.is_empty() || chosen.iter().any(|i| *i >= len) {
                    return Err("Choose at least one segment of this meeting.".to_owned());
                }
                let mut sets: Vec<(&'static str, Value)> = cleared_snapshot()
                    .into_iter()
                    .filter(|(c, _)| !matches!(*c, "end_index" | "omit_ranges_json"))
                    .filter(|(c, _)| *c != "reviewer_retained_json")
                    .collect();
                sets.push(("reviewer_retained_json", Value::Text(store::json(&chosen))));
                if row.cut.is_none() {
                    sets.push(("end_index", Value::Integer(0)));
                }
                store::cas(
                    db.conn(),
                    id,
                    &row.state,
                    state::PREPARING,
                    Reasons::Set(&["edited"]),
                    &sets,
                    now,
                )
                .map_err(|_| STORE_FAILED.to_owned())?
            }
            OutboxAction::Drop => {
                if row.state != state::NEEDS_REVIEW && row.state != state::HELD {
                    return Err(moved());
                }
                transact(db.conn(), |tx| {
                    let applied = store::cas_in(
                        tx,
                        id,
                        &row.state,
                        state::DROPPED,
                        Reasons::Set(&["by_user"]),
                        &[("send_at", Value::Null)],
                        now,
                    )?;
                    if applied {
                        store::record_decision(tx, id, &engine_id, decision::DROPPED, now)?;
                    }
                    Ok(applied)
                })
                .map_err(|_| STORE_FAILED.to_owned())?
            }
            OutboxAction::WrongEvent => {
                // The person says the calendar match was wrong: stop for good,
                // from any state where nothing has gone out yet. Its decision
                // is logged and never counts for the unlock (D19): it is about
                // the calendar, not the engine. A dropped row is never
                // re-created, so the meeting stays unreleased.
                let open = [
                    state::PREPARING,
                    state::NEEDS_REVIEW,
                    state::SCHEDULED,
                    state::HELD,
                ];
                if !open.contains(&row.state.as_str()) {
                    return Err(moved());
                }
                transact(db.conn(), |tx| {
                    let applied = store::cas_in(
                        tx,
                        id,
                        &row.state,
                        state::DROPPED,
                        Reasons::Set(&["wrong_event"]),
                        &[("send_at", Value::Null)],
                        now,
                    )?;
                    if applied {
                        store::record_decision(tx, id, &engine_id, decision::WRONG_EVENT, now)?;
                    }
                    Ok(applied)
                })
                .map_err(|_| STORE_FAILED.to_owned())?
            }
            OutboxAction::Cancel => {
                if row.state != state::SCHEDULED {
                    return Err(moved());
                }
                // Wins only if it lands before `scheduled → sending` (5.3).
                // Records no decision (D19).
                store::cas(
                    db.conn(),
                    id,
                    state::SCHEDULED,
                    state::NEEDS_REVIEW,
                    Reasons::Set(&["review_required"]),
                    &[("send_at", Value::Null), ("approved_by", Value::Null)],
                    now,
                )
                .map_err(|_| STORE_FAILED.to_owned())?
            }
            OutboxAction::Retry => {
                if row.state != state::HELD {
                    return Err(moved());
                }
                if Self::inputs_changed(&mut db, &row) {
                    restart_for_inputs(db.conn(), &row, now)
                } else if row.has("send_refused") {
                    // The same snapshot, now (5.3).
                    store::cas(
                        db.conn(),
                        id,
                        state::HELD,
                        state::SCHEDULED,
                        Reasons::Set(&[]),
                        &[("send_at", Value::Integer(now))],
                        now,
                    )
                    .map_err(|_| STORE_FAILED.to_owned())?
                } else {
                    let reviewer = row
                        .cut
                        .as_ref()
                        .is_some_and(|c| c.reviewer_retained.is_some());
                    let sets: Vec<(&'static str, Value)> = cleared_snapshot()
                        .into_iter()
                        .filter(|(c, _)| {
                            !reviewer
                                || !matches!(
                                    *c,
                                    "end_index" | "omit_ranges_json" | "reviewer_retained_json"
                                )
                        })
                        .collect();
                    let reasons: &[&str] = if reviewer { &["edited"] } else { &[] };
                    store::cas(
                        db.conn(),
                        id,
                        state::HELD,
                        state::PREPARING,
                        Reasons::Set(reasons),
                        &sets,
                        now,
                    )
                    .map_err(|_| STORE_FAILED.to_owned())?
                }
            }
        };
        if !applied {
            return Err(moved());
        }
        let row = store::get(db.conn(), id).ok().flatten().ok_or_else(moved)?;
        crate::diag!(
            "  release: meeting {} -> {} (by user)",
            row.meeting_id,
            row.state
        );
        Ok(build_detail(&mut db, &row))
    }
}

fn transact(
    conn: &rusqlite::Connection,
    f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<bool>,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let applied = f(&tx)?;
    tx.commit()?;
    Ok(applied)
}

/// `needs_review → scheduled` now, approved by a person, recording `decision`.
fn schedule_now(
    conn: &rusqlite::Connection,
    row: &ReleaseRow,
    now: i64,
    decision: Option<(&str, &str)>,
) -> bool {
    transact(conn, |tx| {
        let applied = store::cas_in(
            tx,
            &row.meeting_id,
            state::NEEDS_REVIEW,
            state::SCHEDULED,
            Reasons::Set(&[]),
            &[
                ("send_at", Value::Integer(now)),
                ("approved_by", Value::Text("human".to_owned())),
            ],
            now,
        )?;
        if applied && let Some((engine_id, choice)) = decision {
            store::record_decision(tx, &row.meeting_id, engine_id, choice, now)?;
        }
        Ok(applied)
    })
    .unwrap_or(false)
}

fn item(row: &ReleaseRow) -> OutboxItem {
    OutboxItem {
        meeting_id: row.meeting_id.clone(),
        event_title: row.event_title.clone(),
        state: row.state.clone(),
        reasons: row.reasons.clone(),
        recipient_count: row.recipients.len(),
        finding_count: row.findings.len(),
        created_at: row.created_at,
        send_at: row.send_at,
        event_start_ms: row.event_start_ms,
        event_end_ms: row.event_end_ms,
    }
}

fn build_detail(db: &mut Db, row: &ReleaseRow) -> OutboxDetail {
    let segments = load_input(db, row)
        .map(|(input, _)| {
            let retained = row
                .cut
                .as_ref()
                .map(|c| c.retained(input.segments.len()))
                .unwrap_or_default();
            let clock = fotw_store::export::TranscriptClock::new(input.started_at_ms);
            input
                .segments
                .into_iter()
                .map(|s| {
                    let offset = i64::try_from(s.offset_ms).unwrap_or(i64::MAX);
                    OutboxSegment {
                        retained: retained.binary_search(&s.index).is_ok(),
                        index: s.index,
                        offset_ms: s.offset_ms,
                        timestamp: clock.label(offset, offset),
                        speaker: s.speaker,
                        text: s.text,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    OutboxDetail {
        item: item(row),
        recipients: row
            .recipients
            .iter()
            .map(|r| OutboxRecipient {
                address: r.address.clone(),
                display_name: r.display_name.clone(),
            })
            .collect(),
        subject: row.subject.clone().unwrap_or_default(),
        body: row.body.clone().unwrap_or_default(),
        attachment: row.attachment.clone().unwrap_or_default(),
        findings: row
            .findings
            .iter()
            .map(|f| OutboxFinding {
                category: serde_json::to_value(f.category)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                indices: f.indices.clone(),
                bullet: f.bullet.map(|(s, o)| (section_name(s).to_owned(), o)),
                reason: f.reason.clone(),
            })
            .collect(),
        segments,
    }
}

impl ReleaseControl for Control {
    fn settings(&self) -> ReleaseSettingsResponse {
        self.inner.response()
    }

    fn set_settings(&self, settings: ReleaseSettings) -> BoxFuture<'_, ReleaseSettingsResponse> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            // The preflight spawns `gws`; keep it off the async workers.
            let worker = Arc::clone(&inner);
            match tokio::task::spawn_blocking(move || worker.save(settings)).await {
                Ok(response) => response,
                Err(_) => inner.refuse("The release settings could not be saved. Try again."),
            }
        })
    }

    fn meeting_status(&self, meeting_id: &str) -> MeetingReleaseStatus {
        self.inner.status(meeting_id)
    }

    fn outbox(&self) -> Outbox {
        self.inner.outbox()
    }

    fn detail(&self, meeting_id: &str) -> Option<OutboxDetail> {
        self.inner.detail(meeting_id)
    }

    fn act(&self, meeting_id: &str, action: OutboxAction) -> Result<OutboxDetail, String> {
        self.inner.act(meeting_id, action)
    }
}
