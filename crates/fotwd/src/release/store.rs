//! The release tables (#326 5.3, D10, D19), and [`super::SendStore`] /
//! [`super::NoticeStore`] over them.
//!
//! Every state change is a compare-and-set on the row's current state
//! ([`cas`]): a losing transition changes nothing and reports `false`.

use std::sync::{Arc, Mutex};

use fotw_store::Db;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use super::settings::{ATTENTION_KEY, NOTICE_KEY, get_meta_ms, put_meta};
use super::types::{Bullet, Category, Cut, Finding, IndexMap, ReleaseSummary, Section};
use super::{NoticeStore, SendOutcome, SendRow, SendStore, StoreError};
use crate::calendar::release::Recipient;

/// Row states (5.3).
pub mod state {
    /// Running the cut, summary, render and review.
    pub const PREPARING: &str = "preparing";
    /// Waiting for a person.
    pub const NEEDS_REVIEW: &str = "needs_review";
    /// Will send at `send_at`.
    pub const SCHEDULED: &str = "scheduled";
    /// `gws` may be running.
    pub const SENDING: &str = "sending";
    /// Stopped; Retry or Drop.
    pub const HELD: &str = "held";
    /// Terminal.
    pub const SENT: &str = "sent";
    /// Terminal.
    pub const DROPPED: &str = "dropped";
}

/// D10 reason codes stored on rows, in the order the Outbox shows them.
pub const REASON_ORDER: &[&str] = &[
    "send_outcome_unknown",
    "inputs_changed",
    "edited",
    "has_findings",
    "first_time_recipient",
    "many_recipients",
    "mostly_omitted",
    "reenabled_while_scheduled",
    "delayed_while_paused",
    "mode_changed",
    "auto_locked",
    "review_required",
    "model_failed",
    "body_too_large",
    "send_refused",
    "by_user",
    "no_recipients",
];

/// Sort and de-duplicate reason codes into [`REASON_ORDER`].
#[must_use]
pub fn canonical(reasons: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = REASON_ORDER
        .iter()
        .filter(|r| reasons.contains(r))
        .map(|r| (*r).to_owned())
        .collect();
    for r in reasons {
        if !out.iter().any(|o| o == r) {
            out.push((*r).to_owned());
        }
    }
    out
}

/// One `meeting_releases` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRow {
    /// The meeting.
    pub meeting_id: String,
    /// See [`state`].
    pub state: String,
    /// Reason codes.
    pub reasons: Vec<String>,
    /// Calendar store of the chosen event.
    pub source: String,
    /// The event's id in that store.
    pub source_event_id: String,
    /// iCalendar UID.
    pub ical_uid: Option<String>,
    /// Frozen raw calendar title.
    pub event_title: String,
    /// Frozen recipients (D4).
    pub recipients: Vec<Recipient>,
    /// The cut, when one has been made.
    pub cut: Option<Cut>,
    /// The release summary.
    pub summary: Option<ReleaseSummary>,
    /// Rendered subject.
    pub subject: Option<String>,
    /// Rendered body.
    pub body: Option<String>,
    /// Rendered attachment.
    pub attachment: Option<String>,
    /// Attachment file name.
    pub attachment_name: Option<String>,
    /// Payload provenance.
    pub index_map: Option<IndexMap>,
    /// Review findings.
    pub findings: Vec<Finding>,
    /// D14 fingerprint of what was prepared.
    pub input_fingerprint: Option<String>,
    /// Engine id that prepared it.
    pub engine_id: Option<String>,
    /// `human`, `auto` or none.
    pub approved_by: Option<String>,
    /// When it sends.
    pub send_at: Option<i64>,
    /// Addresses used, once sent.
    pub sent_to: Option<Vec<String>>,
    /// When it was sent.
    pub sent_at: Option<i64>,
    /// `gws`'s message id.
    pub message_id: Option<String>,
    /// Created, epoch ms.
    pub created_at: i64,
    /// Last change, epoch ms.
    pub updated_at: i64,
}

impl ReleaseRow {
    /// Whether the row carries `reason`.
    #[must_use]
    pub fn has(&self, reason: &str) -> bool {
        self.reasons.iter().any(|r| r == reason)
    }

    /// The rendered snapshot as the send needs it, if it has one.
    #[must_use]
    pub fn send_row(&self) -> Option<SendRow> {
        Some(SendRow {
            meeting_id: self.meeting_id.clone(),
            recipients: self.recipients.iter().map(|r| r.address.clone()).collect(),
            subject: self.subject.clone()?,
            body: self.body.clone()?,
            attachment_name: self.attachment_name.clone()?,
            attachment: self.attachment.clone()?.into_bytes(),
        })
    }
}

// ---------------------------------------------------------------- JSON shapes

#[derive(Serialize, Deserialize)]
struct BulletJson {
    text: String,
    citations: Vec<usize>,
}

#[derive(Serialize, Deserialize, Default)]
struct SummaryJson {
    summary: Vec<BulletJson>,
    decisions: Vec<BulletJson>,
    action_items: Vec<BulletJson>,
}

#[derive(Serialize, Deserialize, Default)]
struct IndexMapJson {
    attachment_lines: Vec<(usize, usize, usize)>,
    bullets: Vec<(String, usize, Vec<usize>)>,
}

#[derive(Serialize, Deserialize)]
struct FindingJson {
    category: Category,
    indices: Vec<usize>,
    bullet: Option<(String, usize)>,
    reason: String,
}

/// A section's stored name.
#[must_use]
pub const fn section_name(section: Section) -> &'static str {
    match section {
        Section::Summary => "summary",
        Section::Decisions => "decisions",
        Section::ActionItems => "action_items",
    }
}

fn section_from(name: &str) -> Option<Section> {
    match name {
        "summary" => Some(Section::Summary),
        "decisions" => Some(Section::Decisions),
        "action_items" => Some(Section::ActionItems),
        _ => None,
    }
}

fn bullets_json(b: &[Bullet]) -> Vec<BulletJson> {
    b.iter()
        .map(|b| BulletJson {
            text: b.text.clone(),
            citations: b.citations.clone(),
        })
        .collect()
}

fn bullets_from(b: Vec<BulletJson>) -> Vec<Bullet> {
    b.into_iter()
        .map(|b| Bullet {
            text: b.text,
            citations: b.citations,
        })
        .collect()
}

pub(crate) fn summary_to_json(s: &ReleaseSummary) -> String {
    serde_json::to_string(&SummaryJson {
        summary: bullets_json(&s.summary),
        decisions: bullets_json(&s.decisions),
        action_items: bullets_json(&s.action_items),
    })
    .unwrap_or_else(|_| "{}".to_owned())
}

fn summary_from_json(text: &str) -> Option<ReleaseSummary> {
    let j: SummaryJson = serde_json::from_str(text).ok()?;
    Some(ReleaseSummary {
        summary: bullets_from(j.summary),
        decisions: bullets_from(j.decisions),
        action_items: bullets_from(j.action_items),
    })
}

pub(crate) fn index_map_to_json(m: &IndexMap) -> String {
    serde_json::to_string(&IndexMapJson {
        attachment_lines: m.attachment_lines.clone(),
        bullets: m
            .bullets
            .iter()
            .map(|(s, o, c)| (section_name(*s).to_owned(), *o, c.clone()))
            .collect(),
    })
    .unwrap_or_else(|_| "{}".to_owned())
}

fn index_map_from_json(text: &str) -> Option<IndexMap> {
    let j: IndexMapJson = serde_json::from_str(text).ok()?;
    Some(IndexMap {
        attachment_lines: j.attachment_lines,
        bullets: j
            .bullets
            .into_iter()
            .filter_map(|(s, o, c)| Some((section_from(&s)?, o, c)))
            .collect(),
    })
}

pub(crate) fn findings_to_json(f: &[Finding]) -> String {
    let rows: Vec<FindingJson> = f
        .iter()
        .map(|f| FindingJson {
            category: f.category,
            indices: f.indices.clone(),
            bullet: f.bullet.map(|(s, o)| (section_name(s).to_owned(), o)),
            reason: f.reason.clone(),
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_owned())
}

fn findings_from_json(text: &str) -> Vec<Finding> {
    serde_json::from_str::<Vec<FindingJson>>(text)
        .unwrap_or_default()
        .into_iter()
        .map(|f| Finding {
            category: f.category,
            indices: f.indices,
            bullet: f.bullet.and_then(|(s, o)| Some((section_from(&s)?, o))),
            reason: f.reason,
        })
        .collect()
}

pub(crate) fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

// ---------------------------------------------------------------- reads

const COLUMNS: &str = "meeting_id, state, reasons_json, source, source_event_id, ical_uid, \
     event_title, recipients_json, end_index, omit_ranges_json, reviewer_retained_json, \
     summary_json, rendered_subject, rendered_body, rendered_attachment, \
     rendered_attachment_name, index_map_json, findings_json, input_fingerprint, engine_id, \
     approved_by, send_at, sent_to_json, sent_at, message_id, created_at, updated_at";

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReleaseRow> {
    let reasons: String = r.get(2)?;
    let recipients: String = r.get(7)?;
    let end_index: Option<i64> = r.get(8)?;
    let omit: Option<String> = r.get(9)?;
    let reviewer: Option<String> = r.get(10)?;
    let summary: Option<String> = r.get(11)?;
    let index_map: Option<String> = r.get(16)?;
    let findings: Option<String> = r.get(17)?;
    let sent_to: Option<String> = r.get(22)?;
    let reviewer_retained: Option<Vec<usize>> =
        reviewer.and_then(|t| serde_json::from_str(&t).ok());
    let cut = match (end_index, reviewer_retained) {
        (None, None) => None,
        (end, reviewer_retained) => Some(Cut {
            end_index: end.and_then(|e| usize::try_from(e).ok()).unwrap_or(0),
            omit_ranges: omit
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default(),
            reviewer_retained,
        }),
    };
    Ok(ReleaseRow {
        meeting_id: r.get(0)?,
        state: r.get(1)?,
        reasons: serde_json::from_str(&reasons).unwrap_or_default(),
        source: r.get(3)?,
        source_event_id: r.get(4)?,
        ical_uid: r.get(5)?,
        event_title: r.get(6)?,
        recipients: serde_json::from_str(&recipients).unwrap_or_default(),
        cut,
        summary: summary.as_deref().and_then(summary_from_json),
        subject: r.get(12)?,
        body: r.get(13)?,
        attachment: r.get(14)?,
        attachment_name: r.get(15)?,
        index_map: index_map.as_deref().and_then(index_map_from_json),
        findings: findings
            .as_deref()
            .map(findings_from_json)
            .unwrap_or_default(),
        input_fingerprint: r.get(18)?,
        engine_id: r.get(19)?,
        approved_by: r.get(20)?,
        send_at: r.get(21)?,
        sent_to: sent_to.and_then(|t| serde_json::from_str(&t).ok()),
        sent_at: r.get(23)?,
        message_id: r.get(24)?,
        created_at: r.get(25)?,
        updated_at: r.get(26)?,
    })
}

/// One meeting's row.
///
/// # Errors
///
/// The read failed.
pub fn get(conn: &Connection, meeting_id: &str) -> rusqlite::Result<Option<ReleaseRow>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM meeting_releases WHERE meeting_id = ?1"),
        [meeting_id],
        from_row,
    )
    .optional()
}

/// Every row in one of `states`, oldest first.
///
/// # Errors
///
/// The read failed.
pub fn in_states(conn: &Connection, states: &[&str]) -> rusqlite::Result<Vec<ReleaseRow>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM meeting_releases WHERE state = ?1"
    ))?;
    for s in states {
        let rows = stmt.query_map([s], from_row)?;
        for row in rows {
            out.push(row?);
        }
    }
    out.sort_by(|a, b| (a.created_at, &a.meeting_id).cmp(&(b.created_at, &b.meeting_id)));
    Ok(out)
}

/// `scheduled` rows whose `send_at` has come, earliest first.
///
/// # Errors
///
/// The read failed.
pub fn due(conn: &Connection, now_ms: i64, limit: usize) -> rusqlite::Result<Vec<ReleaseRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM meeting_releases
          WHERE state = 'scheduled' AND send_at IS NOT NULL AND send_at <= ?1
          ORDER BY send_at, meeting_id LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![now_ms, i64::try_from(limit).unwrap_or(5)], from_row)?;
    rows.collect()
}

// ---------------------------------------------------------------- writes

/// A new `preparing` row (5.3 "(none) → preparing").
#[derive(Debug, Clone)]
pub struct NewRelease<'a> {
    /// The meeting.
    pub meeting_id: &'a str,
    /// Calendar store.
    pub source: &'a str,
    /// The event's id there.
    pub source_event_id: &'a str,
    /// iCalendar UID.
    pub ical_uid: Option<&'a str>,
    /// Raw calendar title.
    pub event_title: &'a str,
    /// Recipients after the allowed-domain filter.
    pub recipients: &'a [Recipient],
}

/// Create a `preparing` row; `false` when the meeting already has one.
///
/// # Errors
///
/// The write failed.
pub fn insert_preparing(
    conn: &Connection,
    new: &NewRelease<'_>,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO meeting_releases
           (meeting_id, state, reasons_json, source, source_event_id, ical_uid, event_title,
            recipients_json, created_at, updated_at)
         VALUES (?1, 'preparing', '[]', ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![
            new.meeting_id,
            new.source,
            new.source_event_id,
            new.ical_uid,
            new.event_title,
            json(&new.recipients),
            now_ms
        ],
    )?;
    Ok(n == 1)
}

/// How the reasons column changes in a [`cas`].
#[derive(Debug, Clone, Copy)]
pub enum Reasons<'a> {
    /// Keep what is stored.
    Keep,
    /// Replace with exactly these.
    Set(&'a [&'a str]),
}

/// Compare-and-set: move `meeting_id` from `from` to `to`, writing `sets`
/// (static column names only) in the same statement. `false` when the row
/// was not in `from`. Entering `needs_review` or `held` stamps
/// [`ATTENTION_KEY`] in the same transaction (D16).
///
/// # Errors
///
/// The write failed.
pub fn cas(
    conn: &Connection,
    meeting_id: &str,
    from: &str,
    to: &str,
    reasons: Reasons<'_>,
    sets: &[(&'static str, Value)],
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let applied = cas_in(&tx, meeting_id, from, to, reasons, sets, now_ms)?;
    tx.commit()?;
    Ok(applied)
}

/// [`cas`] inside a transaction the caller owns.
///
/// # Errors
///
/// The write failed.
pub fn cas_in(
    tx: &Connection,
    meeting_id: &str,
    from: &str,
    to: &str,
    reasons: Reasons<'_>,
    sets: &[(&'static str, Value)],
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let mut sql = String::from("UPDATE meeting_releases SET state = ?1, updated_at = ?2");
    let mut values: Vec<Value> = vec![Value::Text(to.to_owned()), Value::Integer(now_ms)];
    if let Reasons::Set(r) = reasons {
        values.push(Value::Text(json(&canonical(r))));
        sql.push_str(&format!(", reasons_json = ?{}", values.len()));
    }
    for (column, value) in sets {
        debug_assert!(column.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        values.push(value.clone());
        sql.push_str(&format!(", {column} = ?{}", values.len()));
    }
    values.push(Value::Text(meeting_id.to_owned()));
    let id_at = values.len();
    values.push(Value::Text(from.to_owned()));
    sql.push_str(&format!(
        " WHERE meeting_id = ?{id_at} AND state = ?{}",
        values.len()
    ));
    let n = tx.execute(&sql, rusqlite::params_from_iter(values))?;
    if n == 1 && (to == state::NEEDS_REVIEW || to == state::HELD) {
        put_meta(tx, ATTENTION_KEY, &now_ms.to_string(), now_ms)?;
    }
    Ok(n == 1)
}

/// Optional text as a SQL value.
#[must_use]
pub fn text(v: Option<String>) -> Value {
    v.map_or(Value::Null, Value::Text)
}

/// The columns that hold a prepared snapshot, cleared when preparation
/// restarts from the cut.
#[must_use]
pub fn cleared_snapshot() -> Vec<(&'static str, Value)> {
    [
        "end_index",
        "omit_ranges_json",
        "reviewer_retained_json",
        "summary_json",
        "rendered_subject",
        "rendered_body",
        "rendered_attachment",
        "rendered_attachment_name",
        "index_map_json",
        "findings_json",
        "approved_by",
        "send_at",
    ]
    .into_iter()
    .map(|c| (c, Value::Null))
    .collect()
}

/// A D19 decision.
pub mod decision {
    /// Approve with no Edit cuts.
    pub const APPROVED_UNCHANGED: &str = "approved_unchanged";
    /// Approve after Edit cuts.
    pub const APPROVED_EDITED: &str = "approved_edited";
    /// Drop from review or held.
    pub const DROPPED: &str = "dropped";
    /// Scheduled by auto.
    pub const AUTO: &str = "auto";
    /// Send again anyway; not counted for the unlock.
    pub const SENT_AGAIN: &str = "sent_again";
}

/// Append a decision (D19).
///
/// # Errors
///
/// The write failed.
pub fn record_decision(
    conn: &Connection,
    meeting_id: &str,
    engine_id: &str,
    decision: &str,
    at_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO release_decisions (id, meeting_id, engine_id, decision, at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![fotw_store::new_id(), meeting_id, engine_id, decision, at_ms],
    )?;
    Ok(())
}

/// `(human decisions, approved_unchanged)` for `engine_id` (D11).
///
/// # Errors
///
/// The read failed.
pub fn decision_counts(conn: &Connection, engine_id: &str) -> rusqlite::Result<(i64, i64)> {
    conn.query_row(
        "SELECT
           COALESCE(SUM(decision IN ('approved_unchanged','approved_edited','dropped')), 0),
           COALESCE(SUM(decision = 'approved_unchanged'), 0)
         FROM release_decisions WHERE engine_id = ?1",
        [engine_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// Whether every address has been sent a release before (D19 ledger).
///
/// # Errors
///
/// The read failed.
pub fn all_known(conn: &Connection, addresses: &[String]) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM release_sent_addresses WHERE address = ?1")?;
    for a in addresses {
        if !stmt.exists([a.to_ascii_lowercase()])? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The cached not-eligible reason, if fresh enough or not.
///
/// # Errors
///
/// The read failed.
pub fn eligibility(conn: &Connection, meeting_id: &str) -> rusqlite::Result<Option<(String, i64)>> {
    conn.query_row(
        "SELECT reason, evaluated_at FROM release_eligibility WHERE meeting_id = ?1",
        [meeting_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// Cache a not-eligible reason.
///
/// # Errors
///
/// The write failed.
pub fn put_eligibility(
    conn: &Connection,
    meeting_id: &str,
    reason: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO release_eligibility (meeting_id, reason, evaluated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(meeting_id) DO UPDATE SET reason = excluded.reason,
           evaluated_at = excluded.evaluated_at",
        params![meeting_id, reason, now_ms],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- send side

/// [`SendStore`] and [`NoticeStore`] over the shared library connection.
/// The mutex is held for one statement or transaction, never across `gws`.
pub struct DbStore {
    db: Arc<Mutex<Db>>,
    now: super::worker::Clock,
}

impl DbStore {
    /// Over `db`, stamping with `now`.
    #[must_use]
    pub fn new(db: Arc<Mutex<Db>>, now: super::worker::Clock) -> Self {
        Self { db, now }
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, StoreError> {
        let db = self.db.lock().map_err(|_| StoreError)?;
        f(db.conn()).map_err(|_| StoreError)
    }
}

impl SendStore for DbStore {
    fn claim_sending(&self, meeting_id: &str) -> Result<bool, StoreError> {
        let now = (self.now)();
        self.with(|c| {
            cas(
                c,
                meeting_id,
                state::SCHEDULED,
                state::SENDING,
                Reasons::Keep,
                &[],
                now,
            )
        })
    }

    fn settle(&self, meeting_id: &str, outcome: &SendOutcome) -> Result<bool, StoreError> {
        let now = (self.now)();
        self.with(|c| match outcome {
            SendOutcome::LostRace => Ok(false),
            SendOutcome::NoRecipients => cas(
                c,
                meeting_id,
                state::SCHEDULED,
                state::DROPPED,
                Reasons::Set(&["no_recipients"]),
                &[],
                now,
            ),
            SendOutcome::GwsUnavailable => cas(
                c,
                meeting_id,
                state::SENDING,
                state::SCHEDULED,
                Reasons::Keep,
                &[],
                now,
            ),
            SendOutcome::SendRefused { .. } => cas(
                c,
                meeting_id,
                state::SENDING,
                state::HELD,
                Reasons::Set(&["send_refused"]),
                &[],
                now,
            ),
            SendOutcome::OutcomeUnknown { .. } => cas(
                c,
                meeting_id,
                state::SENDING,
                state::NEEDS_REVIEW,
                Reasons::Set(&["send_outcome_unknown"]),
                &[],
                now,
            ),
            SendOutcome::Sent {
                sent_to,
                message_id,
            } => {
                let tx = c.unchecked_transaction()?;
                // The ledger is written whatever the row says: `gws` ran.
                for address in sent_to {
                    tx.execute(
                        "INSERT OR IGNORE INTO release_sent_addresses (address, first_sent_ms)
                         VALUES (?1, ?2)",
                        params![address.to_ascii_lowercase(), now],
                    )?;
                }
                let applied = cas_in(
                    &tx,
                    meeting_id,
                    state::SENDING,
                    state::SENT,
                    Reasons::Set(&[]),
                    &[
                        ("sent_to_json", Value::Text(json(sent_to))),
                        ("sent_at", Value::Integer(now)),
                        ("message_id", text(message_id.clone())),
                    ],
                    now,
                )?;
                tx.commit()?;
                Ok(applied)
            }
        })
    }
}

impl NoticeStore for DbStore {
    fn waiting_count(&self) -> Result<usize, StoreError> {
        self.with(|c| {
            c.query_row(
                "SELECT count(*) FROM meeting_releases WHERE state IN ('needs_review','held')",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .map(|n| usize::try_from(n).unwrap_or(0))
    }

    fn last_attention_ms(&self) -> Result<Option<i64>, StoreError> {
        self.with(|c| get_meta_ms(c, ATTENTION_KEY))
    }

    fn last_notice_ms(&self) -> Result<Option<i64>, StoreError> {
        self.with(|c| get_meta_ms(c, NOTICE_KEY))
    }

    fn record_notice(&self, at_ms: i64) -> Result<(), StoreError> {
        self.with(|c| put_meta(c, NOTICE_KEY, &at_ms.to_string(), at_ms))
    }
}
