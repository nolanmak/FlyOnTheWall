//! Web contract for attendee release (#326 5.1, 5.7).
//!
//! The daemon implements [`ReleaseControl`] (#331); this crate serves the
//! routes and the Outbox UI against it (#332). Change a type here only together
//! with both. Every error string is safe user-facing text with no content or
//! addresses (D15).

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

/// Release mode (D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseMode {
    /// Every prepared release waits for a person.
    #[default]
    Approval,
    /// Clean releases with known recipients send after the delay.
    Auto,
}

/// Stored release settings (5.1). Device-local; never archived (D17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleaseSettings {
    /// Release on or off.
    pub enabled: bool,
    /// Approval or auto.
    pub mode: ReleaseMode,
    /// Exact lowercase domains.
    pub allowed_domains: Vec<String>,
    /// `GOOGLE_WORKSPACE_CLI_CONFIG_DIR` for `gws`.
    pub gws_profile_dir: String,
    /// Optional explicit `gws` path; empty resolves like `gh`.
    pub gws_binary: String,
    /// 0–120.
    pub send_delay_minutes: u32,
    /// Optional owner notice address; domain must be allowed.
    pub notify_address: String,
    /// Server-set cutoff (D2); a client value is ignored.
    pub release_since_ms: i64,
    /// The sender verified by the last passing preflight; server-set.
    pub verified_sender: String,
}

impl Default for ReleaseSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ReleaseMode::Approval,
            allowed_domains: Vec::new(),
            gws_profile_dir: String::new(),
            gws_binary: String::new(),
            send_delay_minutes: 15,
            notify_address: String::new(),
            release_since_ms: 0,
            verified_sender: String::new(),
        }
    }
}

/// `GET/POST /api/settings/release` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseSettingsResponse {
    /// What is stored; after a refused save, what is still stored.
    pub settings: ReleaseSettings,
    /// Why a save was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether auto mode is currently unlocked (D11).
    pub auto_unlocked: bool,
    /// Worker pause code, if paused (D10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
}

/// `GET /api/meetings/{id}/release` (5.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingReleaseStatus {
    /// `not_eligible`, `preparing`, `needs_review`, `scheduled`, `sending`,
    /// `held`, `sent` or `dropped`.
    pub state: String,
    /// Reason codes (D10); for `not_eligible`, the one reason.
    pub reasons: Vec<String>,
    /// `sent_to` length once sent, the frozen list length before.
    pub recipient_count: usize,
    /// Epoch ms, when scheduled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_at: Option<i64>,
    /// Epoch ms, when sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<i64>,
    /// The matched calendar event's frozen title; only when a row exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_title: Option<String>,
    /// The matched event's scheduled start, epoch ms; `None` without a row
    /// or on a row created before times were stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_start_ms: Option<i64>,
    /// The matched event's scheduled end, epoch ms; `None` likewise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_end_ms: Option<i64>,
}

/// One frozen recipient as the Outbox shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxRecipient {
    /// Address.
    pub address: String,
    /// Display name, if any.
    pub display_name: Option<String>,
}

/// One finding as the Outbox shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxFinding {
    /// One of the six D8 categories, snake_case.
    pub category: String,
    /// Retained source indices.
    pub indices: Vec<usize>,
    /// `(section, ordinal)` with section `summary`, `decisions` or
    /// `action_items`.
    pub bullet: Option<(String, usize)>,
    /// One line.
    pub reason: String,
}

/// One segment the reviewer can toggle in Edit cuts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxSegment {
    /// Source index.
    pub index: usize,
    /// Offset from capture start, ms.
    pub offset_ms: u64,
    /// Speaker label.
    pub speaker: String,
    /// Source text.
    pub text: String,
    /// Whether the current cut retains it.
    pub retained: bool,
}

/// `GET /api/release/outbox` row summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxItem {
    /// Meeting id.
    pub meeting_id: String,
    /// Frozen calendar title.
    pub event_title: String,
    /// `needs_review`, `scheduled` or `held`.
    pub state: String,
    /// Reason codes.
    pub reasons: Vec<String>,
    /// Frozen recipient count.
    pub recipient_count: usize,
    /// Finding count.
    pub finding_count: usize,
    /// Epoch ms.
    pub created_at: i64,
    /// Epoch ms, when scheduled.
    pub send_at: Option<i64>,
    /// The matched event's scheduled start, epoch ms; `None` on rows created
    /// before times were stored.
    #[serde(default)]
    pub event_start_ms: Option<i64>,
    /// The matched event's scheduled end, epoch ms; `None` likewise.
    #[serde(default)]
    pub event_end_ms: Option<i64>,
}

/// `GET /api/release/outbox` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbox {
    /// Rows, oldest first.
    pub items: Vec<OutboxItem>,
    /// `needs_review` + `held` count (D16 badge).
    pub badge: usize,
    /// Worker pause code, if paused.
    pub paused: Option<String>,
}

/// `GET /api/release/outbox/{meeting_id}`: the stored snapshot (5.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxDetail {
    /// The row summary.
    pub item: OutboxItem,
    /// Frozen recipients.
    pub recipients: Vec<OutboxRecipient>,
    /// Stored rendered subject.
    pub subject: String,
    /// Stored rendered body.
    pub body: String,
    /// Stored rendered attachment.
    pub attachment: String,
    /// Findings.
    pub findings: Vec<OutboxFinding>,
    /// Every source segment with its current retained flag, for Edit cuts.
    pub segments: Vec<OutboxSegment>,
}

/// `POST /api/release/outbox/{meeting_id}` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum OutboxAction {
    /// Approve & send.
    Approve,
    /// Send again anyway (only for `send_outcome_unknown`).
    SendAgain,
    /// Replace the cut with this reviewer retained set.
    EditCuts {
        /// The chosen retained indices; non-empty.
        retained: Vec<usize>,
    },
    /// Drop.
    Drop,
    /// Cancel a scheduled send.
    Cancel,
    /// Retry a held row.
    Retry,
    /// The calendar match is wrong: stop the release for good. Allowed from
    /// `preparing`, `needs_review`, `scheduled` and `held`.
    WrongEvent,
}

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Implemented by the daemon (#331); served by this crate (#332).
pub trait ReleaseControl: Send + Sync {
    /// Current settings.
    fn settings(&self) -> ReleaseSettingsResponse;
    /// Validate and store; a refusal returns the stored settings plus `error`.
    fn set_settings(&self, settings: ReleaseSettings) -> BoxFuture<'_, ReleaseSettingsResponse>;
    /// One meeting's release line.
    fn meeting_status(&self, meeting_id: &str) -> MeetingReleaseStatus;
    /// The Outbox list.
    fn outbox(&self) -> Outbox;
    /// One row's stored snapshot, or `None` when it has no Outbox row.
    fn detail(&self, meeting_id: &str) -> Option<OutboxDetail>;
    /// Apply an action. `Err` is safe user-facing text (e.g. the row moved on).
    fn act(&self, meeting_id: &str, action: OutboxAction) -> Result<OutboxDetail, String>;
}

impl std::fmt::Debug for dyn ReleaseControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReleaseControl(<redacted>)")
    }
}
