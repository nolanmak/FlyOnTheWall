//! Google Calendar through Composio: optional, and off until the user saves a
//! Composio key and connects an account from Settings.
//!
//! # What Composio sees
//!
//! Composio holds the Google sign-in and makes the Calendar API calls. When a
//! recording is saved, it is told the recording's time window and returns the
//! events in it, from which only title, times, status and the user's own
//! response are kept. Audio, transcripts and summaries never go to Composio.
//! This is a hosted relay, which docs/REQUIREMENTS.md §2 otherwise rules out,
//! so it exists only behind an explicit opt-in with the disclosure shown in
//! Settings (KEY-04).
//!
//! # Calls
//!
//! Composio REST API v3.1, `x-api-key` auth, host `backend.composio.dev`
//! (on [`crate::transport::ALLOWED_HOSTS`]). Google sign-in uses
//! Composio-managed OAuth, so the user needs no Google Cloud project.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use fotw_secrets::{KeyStore, Provider, SecretKey, SecretString, SecretsError};
use fotw_store::Db;
use fotw_web::calendar::{
    AccountState, CalendarAction, CalendarControl, CalendarSettingsResponse, GoogleAccount,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CalendarLookup, Participation, ScheduledEvent};

/// Every Composio call starts with this.
pub const BASE: &str = "https://backend.composio.dev/api/v3.1";

/// The toolkit slug for Google Calendar.
pub const TOOLKIT: &str = "googlecalendar";

/// Reads every calendar on the account in one call.
pub const EVENTS_TOOL: &str = "GOOGLECALENDAR_EVENTS_LIST_ALL_CALENDARS";

/// Lists the account's calendars; the primary one's id is the address.
pub const CALENDARS_TOOL: &str = "GOOGLECALENDAR_LIST_CALENDARS";

/// Where the non-secret connection state lives in the library.
pub const SETTINGS_KEY: &str = "calendar_composio";

/// How long one Composio call may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A connected Google account, as stored.
pub type Account = GoogleAccount;

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// GET.
    Get,
    /// POST with a JSON body.
    Post,
    /// DELETE.
    Delete,
}

/// One Composio request. Never holds the key.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Method.
    pub method: Method,
    /// Full URL, always under [`BASE`].
    pub url: String,
    /// JSON body, for POST.
    pub body: Option<Value>,
}

impl Request {
    fn get(path: &str) -> Self {
        Self {
            method: Method::Get,
            url: format!("{BASE}{path}"),
            body: None,
        }
    }
    fn post(path: &str, body: Value) -> Self {
        Self {
            method: Method::Post,
            url: format!("{BASE}{path}"),
            body: Some(body),
        }
    }
    fn delete(path: &str) -> Self {
        Self {
            method: Method::Delete,
            url: format!("{BASE}{path}"),
            body: None,
        }
    }
}

/// Sends Composio requests. A trait so tests never reach the network.
pub trait ComposioHttp: Send + Sync {
    /// Send `request` with `key`, returning the status and the JSON body
    /// (`Value::Null` when there is none).
    ///
    /// # Errors
    /// The request could not be made at all.
    fn send(&self, key: &str, request: &Request) -> Result<(u16, Value), String>;
}

/// The real client: blocking reqwest, allowlisted host, no proxy.
///
/// The client is built on first use, which is always on a blocking thread:
/// reqwest's blocking client may not be created or dropped inside the async
/// runtime.
#[derive(Default)]
pub struct ReqwestHttp {
    client: OnceLock<Result<reqwest::blocking::Client, String>>,
}

impl ComposioHttp for ReqwestHttp {
    fn send(&self, key: &str, request: &Request) -> Result<(u16, Value), String> {
        if !crate::transport::is_allowed(&request.url) {
            return Err("refusing to contact a host that is not on the allowlist".into());
        }
        let client = self
            .client
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .timeout(REQUEST_TIMEOUT)
                    .no_proxy()
                    // A redirect could carry the key to another host.
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let builder = match request.method {
            Method::Get => client.get(&request.url),
            Method::Post => client.post(&request.url),
            Method::Delete => client.delete(&request.url),
        }
        .header("x-api-key", key)
        .header("accept", "application/json");
        let builder = match &request.body {
            Some(body) => builder.json(body),
            None => builder,
        };
        let response = builder
            .send()
            .map_err(|e| format!("could not reach Composio: {}", e.without_url()))?;
        let status = response.status().as_u16();
        let text = response.text().unwrap_or_default();
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }
}

/// Send, and turn non-2xx answers into a message for the user.
fn call(http: &dyn ComposioHttp, key: &str, request: &Request) -> Result<Value, String> {
    let (status, body) = http.send(key, request)?;
    if (200..=299).contains(&status) {
        Ok(body)
    } else {
        Err(status_error(status, &body))
    }
}

/// A non-2xx answer, in words.
fn status_error(status: u16, body: &Value) -> String {
    if matches!(status, 401 | 403) {
        return "Composio rejected the API key. Check it in Settings.".into();
    }
    format!(
        "Composio answered {status}{}",
        message_of(body)
            .map(|m| format!(": {m}"))
            .unwrap_or_default()
    )
}

/// A short error message from a Composio body, if it has one.
fn message_of(body: &Value) -> Option<String> {
    let found = body
        .get("error")
        .and_then(|e| {
            e.as_str()
                .map(str::to_owned)
                .or_else(|| e.get("message").and_then(Value::as_str).map(str::to_owned))
        })
        .or_else(|| {
            body.get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })?;
    Some(found.chars().take(200).collect())
}

// --- Dates -------------------------------------------------------------------

/// Epoch milliseconds as `YYYY-MM-DDTHH:MM:SSZ`.
#[must_use]
pub fn rfc3339_utc(epoch_ms: i64) -> String {
    let (y, m, d) = crate::okf::ymd_utc(epoch_ms);
    let secs = epoch_ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)` as epoch milliseconds.
#[must_use]
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { text.get(from..to)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0i64;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let padded = format!("{digits:0<3}");
        millis = padded[..3].parse().ok()?;
        rest = &fraction[digits.len()..];
    }
    let offset_min = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (oh * 60 + om)
        }
    };
    let days = crate::okf::days_from_civil(y, u32::try_from(mo).ok()?, u32::try_from(d).ok()?);
    let local = ((days * 24 + h) * 60 + mi) * 60 + s;
    Some((local - offset_min * 60) * 1000 + millis)
}

/// `YYYY-MM-DD` as epoch milliseconds at UTC midnight.
fn parse_date(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = text.get(0..4)?.parse().ok()?;
    let m: u32 = text.get(5..7)?.parse().ok()?;
    let d: u32 = text.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(crate::okf::days_from_civil(y, m, d) * 86_400_000)
}

// --- Events ------------------------------------------------------------------

/// Every Google Calendar event anywhere in a tool response.
///
/// The tool's `data` shape is not documented, so events are found by shape: an
/// object with an `id` and a `start`/`end` holding `dateTime` or `date`. The
/// same event reached through two calendars is kept once.
#[must_use]
pub fn events_from(data: &Value) -> Vec<ScheduledEvent> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    walk(data, &mut found, &mut seen, 0);
    found
}

fn walk(
    value: &Value,
    found: &mut Vec<ScheduledEvent>,
    seen: &mut HashSet<(String, i64)>,
    depth: usize,
) {
    // Deep enough for any sane wrapper, shallow enough to never recurse away.
    if depth > 12 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                walk(item, found, seen, depth + 1);
            }
        }
        Value::Object(map) => {
            if let Some(event) = as_event(value) {
                if seen.insert((event.uid.clone(), event.start_ms)) {
                    found.push(event);
                }
                return;
            }
            for child in map.values() {
                walk(child, found, seen, depth + 1);
            }
        }
        _ => {}
    }
}

fn as_event(value: &Value) -> Option<ScheduledEvent> {
    let uid = value
        .get("id")
        .or_else(|| value.get("iCalUID"))
        .and_then(Value::as_str)?
        .to_owned();
    let (start_ms, all_day) = when(value.get("start")?)?;
    let (end_ms, _) = when(value.get("end")?)?;
    let me = value
        .get("attendees")
        .and_then(Value::as_array)
        .and_then(|people| {
            people
                .iter()
                .find(|p| p.get("self").and_then(Value::as_bool) == Some(true))
        });
    let participation = match me
        .and_then(|p| p.get("responseStatus"))
        .and_then(Value::as_str)
    {
        Some("accepted") => Participation::Accepted,
        Some("declined") => Participation::Declined,
        Some("tentative") => Participation::Tentative,
        _ => Participation::Unknown,
    };
    Some(ScheduledEvent {
        uid,
        title: value
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        start_ms,
        end_ms,
        all_day,
        canceled: value.get("status").and_then(Value::as_str) == Some("cancelled"),
        participation,
        source: "composio",
    })
}

/// A Google `start`/`end`: `(epoch ms, all_day)`.
fn when(value: &Value) -> Option<(i64, bool)> {
    if let Some(at) = value.get("dateTime").and_then(Value::as_str) {
        return parse_rfc3339(at).map(|ms| (ms, false));
    }
    value
        .get("date")
        .and_then(Value::as_str)
        .and_then(parse_date)
        .map(|ms| (ms, true))
}

// --- Composio calls ----------------------------------------------------------

/// The Composio-managed Google Calendar auth config, creating one if needed.
///
/// # Errors
/// Composio refused or could not be reached.
pub fn ensure_auth_config(http: &dyn ComposioHttp, key: &str) -> Result<String, String> {
    let listed = call(
        http,
        key,
        &Request::get(&format!(
            "/auth_configs?toolkit_slug={TOOLKIT}&is_composio_managed=true&limit=50"
        )),
    )?;
    let existing = listed
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|item| {
            item.get("status")
                .and_then(Value::as_str)
                .is_none_or(|s| s.eq_ignore_ascii_case("ENABLED"))
        })
        .and_then(|item| item.get("id").and_then(Value::as_str));
    if let Some(id) = existing {
        return Ok(id.to_owned());
    }
    let created = call(
        http,
        key,
        &Request::post(
            "/auth_configs",
            json!({
                "toolkit": {"slug": TOOLKIT},
                "auth_config": {"type": "use_composio_managed_auth", "name": "FlyOnTheWall"},
            }),
        ),
    )?;
    created
        .pointer("/auth_config/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Composio did not return an auth config".into())
}

/// A Google sign-in page for a new connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Where the user signs in.
    pub redirect_url: String,
    /// The connected account it will become.
    pub account_id: String,
}

/// Start connecting a Google account.
///
/// # Errors
/// Composio refused, or answered with something that is not an `https` link.
pub fn start_link(
    http: &dyn ComposioHttp,
    key: &str,
    auth_config_id: &str,
    user_id: &str,
) -> Result<Link, String> {
    let body = call(
        http,
        key,
        &Request::post(
            "/connected_accounts/link",
            json!({"auth_config_id": auth_config_id, "user_id": user_id}),
        ),
    )?;
    let field = |name: &str| body.get(name).and_then(Value::as_str).map(str::to_owned);
    let (Some(redirect_url), Some(account_id)) =
        (field("redirect_url"), field("connected_account_id"))
    else {
        return Err("Composio did not return a sign-in link".into());
    };
    // The page opens this in a new tab, so it must be a web page and nothing
    // else.
    if !redirect_url.starts_with("https://") {
        return Err("Composio returned an unexpected sign-in link".into());
    }
    Ok(Link {
        redirect_url,
        account_id,
    })
}

/// Where one connected account stands.
///
/// # Errors
/// Composio refused or could not be reached. A deleted account is `Failed`.
pub fn account_state(
    http: &dyn ComposioHttp,
    key: &str,
    account_id: &str,
) -> Result<AccountState, String> {
    let (status, body) = http.send(
        key,
        &Request::get(&format!("/connected_accounts/{account_id}")),
    )?;
    if status == 404 {
        return Ok(AccountState::Failed);
    }
    if !(200..=299).contains(&status) {
        return Err(status_error(status, &body));
    }
    Ok(
        match body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase()
            .as_str()
        {
            "ACTIVE" => AccountState::Active,
            "INITIATED" | "INITIALIZING" => AccountState::Pending,
            _ => AccountState::Failed,
        },
    )
}

/// Disconnect an account at Composio. An account already gone is fine.
///
/// # Errors
/// Composio refused or could not be reached.
pub fn delete_account(http: &dyn ComposioHttp, key: &str, account_id: &str) -> Result<(), String> {
    let (status, body) = http.send(
        key,
        // Revoke the Google grant too: disconnecting should leave Composio
        // with no access to the calendar.
        &Request::delete(&format!(
            "/connected_accounts/{account_id}?revoke_on_delete=true"
        )),
    )?;
    match status {
        200..=299 | 404 => Ok(()),
        _ => Err(status_error(status, &body)),
    }
}

/// Events on every calendar of `account_id` between `start_ms` and `end_ms`.
///
/// # Errors
/// Composio refused, could not be reached, or the tool call failed.
pub fn list_events(
    http: &dyn ComposioHttp,
    key: &str,
    account_id: &str,
    user_id: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<ScheduledEvent>, String> {
    let body = call(
        http,
        key,
        &Request::post(
            &format!("/tools/execute/{EVENTS_TOOL}"),
            json!({
                "connected_account_id": account_id,
                // Composio refuses to run a tool on an account unless the
                // request also names the user it belongs to.
                "user_id": user_id,
                "version": "latest",
                "arguments": {
                    "time_min": rfc3339_utc(start_ms),
                    "time_max": rfc3339_utc(end_ms),
                    "single_events": true,
                    "response_detail": "full",
                },
            }),
        ),
    )?;
    if body.get("successful").and_then(Value::as_bool) == Some(false) {
        return Err(format!(
            "the calendar read failed{}",
            message_of(&body)
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        ));
    }
    Ok(events_from(body.get("data").unwrap_or(&body)))
}

/// The Google address of `account_id`: the id of its primary calendar.
///
/// `Ok(None)` when no primary calendar with an address-shaped id is listed.
///
/// # Errors
/// Composio refused, could not be reached, or the tool call failed.
pub fn primary_email(
    http: &dyn ComposioHttp,
    key: &str,
    account_id: &str,
    user_id: &str,
) -> Result<Option<String>, String> {
    let body = call(
        http,
        key,
        &Request::post(
            &format!("/tools/execute/{CALENDARS_TOOL}"),
            json!({
                "connected_account_id": account_id,
                "user_id": user_id,
                "version": "latest",
                "arguments": {"max_results": 250},
            }),
        ),
    )?;
    if body.get("successful").and_then(Value::as_bool) == Some(false) {
        return Err(format!(
            "listing calendars failed{}",
            message_of(&body)
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        ));
    }
    let calendars = body
        .pointer("/data/calendars")
        .or_else(|| body.pointer("/data/items"))
        .and_then(Value::as_array);
    Ok(calendars
        .into_iter()
        .flatten()
        .find(|c| c.get("primary").and_then(Value::as_bool) == Some(true))
        .and_then(|c| c.get("id").and_then(Value::as_str))
        .filter(|id| id.contains('@') && id.len() <= 254)
        .map(str::to_owned))
}

// --- Stored state ------------------------------------------------------------

/// The connection state kept in the library. No secrets: the key is in the
/// keychain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposioSettings {
    /// This install's Composio user id, minted on first connect.
    pub user_id: String,
    /// The Google Calendar auth config, once found or created.
    pub auth_config_id: Option<String>,
    /// The user accepted the disclosure.
    pub acknowledged_egress: bool,
    /// Connected Google accounts.
    pub accounts: Vec<Account>,
}

impl ComposioSettings {
    /// The stored state, or the default when there is none or it is unreadable.
    #[must_use]
    pub fn read(db: &Db) -> Self {
        Self::try_read(db).unwrap_or_default()
    }

    /// The stored state; the default only when nothing is stored yet.
    ///
    /// # Errors
    /// The row exists but cannot be read. Callers that write must stop here
    /// rather than overwrite the user's connected accounts with nothing.
    pub fn try_read(db: &Db) -> Result<Self, String> {
        match db.get_setting(SETTINGS_KEY) {
            Ok(None) => Ok(Self::default()),
            Ok(Some(json)) => serde_json::from_str(&json).map_err(|_| {
                "The saved Google Calendar connection could not be read. Nothing was changed."
                    .to_owned()
            }),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Store this state.
    ///
    /// # Errors
    /// The library write failed.
    pub fn write(&self, db: &mut Db) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        db.put_setting(SETTINGS_KEY, &json)
            .map_err(|e| e.to_string())
    }

    /// Whether any account can be read.
    #[must_use]
    pub fn has_active(&self) -> bool {
        self.accounts
            .iter()
            .any(|a| a.status == AccountState::Active)
    }
}

// --- Lookup ------------------------------------------------------------------

/// Reads the Composio key; `Ok(None)` when none is saved.
pub type KeySource = Arc<dyn Fn() -> Result<Option<String>, String> + Send + Sync>;

/// The connected Google accounts as a calendar store.
pub struct ComposioCalendar {
    http: Arc<dyn ComposioHttp>,
    key: KeySource,
    user_id: String,
    accounts: Vec<Account>,
    deadline: Duration,
}

/// How long the Google accounts get, together, to answer one lookup: inside
/// [`super::MANY_DEADLINE`].
pub const ACCOUNTS_DEADLINE: Duration = Duration::from_secs(5);

impl ComposioCalendar {
    /// Read `accounts` through `http`, with the key from `key`.
    #[must_use]
    pub fn new(
        http: Arc<dyn ComposioHttp>,
        key: KeySource,
        user_id: String,
        accounts: Vec<Account>,
    ) -> Self {
        Self {
            http,
            key,
            user_id,
            accounts,
            deadline: ACCOUNTS_DEADLINE,
        }
    }

    /// The same store with a different shared deadline for its accounts.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

impl CalendarLookup for ComposioCalendar {
    fn source(&self) -> &'static str {
        "composio"
    }

    fn events_overlapping(
        &self,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<ScheduledEvent>, String> {
        let active: Vec<&Account> = self
            .accounts
            .iter()
            .filter(|a| a.status == AccountState::Active)
            .collect();
        if active.is_empty() {
            return Ok(Vec::new());
        }
        // Read inside the lookup, so a slow keychain counts against the
        // lookup's deadline rather than holding the save.
        let Some(key) = (self.key)()? else {
            return Ok(Vec::new());
        };
        // Side by side, under one deadline: an account that stalls must not
        // keep the next one from being read.
        let until = std::time::Instant::now() + self.deadline;
        let (tx, rx) = std::sync::mpsc::channel();
        for account in &active {
            let (http, key, id, user, tx) = (
                Arc::clone(&self.http),
                key.clone(),
                account.id.clone(),
                self.user_id.clone(),
                tx.clone(),
            );
            let _ = std::thread::Builder::new()
                .name("fotw-composio-account".into())
                .spawn(move || {
                    let _ = tx.send(list_events(
                        http.as_ref(),
                        &key,
                        &id,
                        &user,
                        start_ms,
                        end_ms,
                    ));
                });
        }
        drop(tx);
        let (mut events, mut read) = (Vec::new(), 0);
        for _ in 0..active.len() {
            let left = until.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(Ok(found)) => {
                    read += 1;
                    events.extend(found);
                }
                Ok(Err(_)) => {}
                Err(_) => break,
            }
        }
        if read == 0 {
            return Err(format!(
                "none of {} Google account(s) could be read",
                active.len()
            ));
        }
        Ok(events)
    }
}

/// The key, read from the process keychain.
#[must_use]
pub fn keychain_key() -> KeySource {
    Arc::new(|| {
        let store = crate::secrets::keystore().map_err(|e| e.to_string())?;
        read_key(store)
    })
}

fn read_key(store: &dyn KeyStore) -> Result<Option<String>, String> {
    match store.get(SecretKey::ApiKey(Provider::Composio)) {
        Ok(secret) => Ok(Some(secret.expose().to_owned())),
        Err(SecretsError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// The Composio store for this library, if any account is connected.
#[must_use]
pub fn for_library(db: &Db) -> Option<ComposioCalendar> {
    let settings = ComposioSettings::read(db);
    settings.has_active().then(|| {
        ComposioCalendar::new(
            Arc::new(ReqwestHttp::default()),
            keychain_key(),
            settings.user_id,
            settings.accounts,
        )
    })
}

// --- Settings control --------------------------------------------------------

/// The Settings section's daemon side.
pub struct CalendarSettingsControl {
    db: Mutex<Db>,
    store: &'static dyn KeyStore,
    http: Arc<dyn ComposioHttp>,
    /// Held across each action's read, network calls and write, so two
    /// overlapping actions cannot drop each other's accounts.
    acting: Mutex<()>,
}

impl std::fmt::Debug for CalendarSettingsControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CalendarSettingsControl(<redacted>)")
    }
}

impl CalendarSettingsControl {
    /// A control over its own library handle.
    #[must_use]
    pub fn new(db: Db, store: &'static dyn KeyStore, http: Arc<dyn ComposioHttp>) -> Self {
        Self {
            db: Mutex::new(db),
            store,
            http,
            acting: Mutex::new(()),
        }
    }

    /// The raw stored row. For tests.
    #[doc(hidden)]
    #[must_use]
    pub fn stored_setting_for_tests(&self) -> Option<String> {
        self.lock().get_setting(SETTINGS_KEY).ok().flatten()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn key(&self) -> Result<String, String> {
        read_key(self.store)?.ok_or_else(|| "Save your Composio API key first.".to_owned())
    }
}

fn apple_access() -> String {
    #[cfg(target_os = "macos")]
    {
        super::eventkit::access().label().to_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        "unavailable on this system".to_owned()
    }
}

impl CalendarControl for CalendarSettingsControl {
    fn read(&self) -> CalendarSettingsResponse {
        let settings = ComposioSettings::read(&self.lock());
        CalendarSettingsResponse {
            apple_access: apple_access(),
            composio_key_saved: self
                .store
                .contains(SecretKey::ApiKey(Provider::Composio))
                .unwrap_or(false),
            composio_acknowledged: settings.acknowledged_egress,
            google_accounts: settings.accounts,
            redirect_url: None,
            error: None,
        }
    }

    fn act(&self, action: CalendarAction) -> Result<Option<String>, String> {
        let _acting = self
            .acting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match action {
            CalendarAction::SaveKey { key } => {
                let key = key.trim().to_owned();
                // A cheap, read-only call proves the key before it is kept.
                call(
                    self.http.as_ref(),
                    &key,
                    &Request::get("/auth_configs?limit=1"),
                )?;
                let replaced = read_key(self.store)?.is_some_and(|old| old != key);
                self.store
                    .set(
                        SecretKey::ApiKey(Provider::Composio),
                        &SecretString::from_pasted(key),
                    )
                    .map_err(|e| e.to_string())?;
                // A different key may belong to another Composio project,
                // where the saved auth config does not exist. It is looked
                // up again on the next connect; accounts are kept, because a
                // rotation within one project leaves them valid.
                if replaced {
                    let mut settings = ComposioSettings::try_read(&self.lock())?;
                    if settings.auth_config_id.take().is_some() {
                        settings.write(&mut self.lock())?;
                    }
                }
                Ok(None)
            }
            CalendarAction::ForgetKey => {
                match self.store.delete(SecretKey::ApiKey(Provider::Composio)) {
                    Ok(()) | Err(SecretsError::NotFound { .. }) => Ok(None),
                    Err(e) => Err(e.to_string()),
                }
            }
            CalendarAction::Connect { acknowledge_egress } => {
                let key = self.key()?;
                let mut settings = ComposioSettings::try_read(&self.lock())?;
                if !acknowledge_egress && !settings.acknowledged_egress {
                    return Err("Tick the box to confirm event titles and times may pass \
                                through Composio."
                        .into());
                }
                settings.acknowledged_egress = true;
                if settings.user_id.is_empty() {
                    settings.user_id = format!("fotw-{}", fotw_store::new_id());
                }
                let auth_config_id = match &settings.auth_config_id {
                    Some(id) => id.clone(),
                    None => ensure_auth_config(self.http.as_ref(), &key)?,
                };
                settings.auth_config_id = Some(auth_config_id.clone());
                let link =
                    start_link(self.http.as_ref(), &key, &auth_config_id, &settings.user_id)?;
                settings.accounts.push(Account {
                    id: link.account_id,
                    status: AccountState::Pending,
                    added_at_ms: fotw_store::now_ms(),
                    email: None,
                });
                settings.write(&mut self.lock())?;
                Ok(Some(link.redirect_url))
            }
            CalendarAction::Refresh => {
                let key = self.key()?;
                let mut settings = ComposioSettings::try_read(&self.lock())?;
                let mut failed = None;
                let user_id = settings.user_id.clone();
                for account in &mut settings.accounts {
                    if account.status == AccountState::Pending {
                        match account_state(self.http.as_ref(), &key, &account.id) {
                            Ok(state) => account.status = state,
                            Err(e) => failed = Some(e),
                        }
                    }
                    // Named once, when first seen active, so Settings can
                    // say which Google account each row is.
                    if account.status == AccountState::Active && account.email.is_none() {
                        match primary_email(self.http.as_ref(), &key, &account.id, &user_id) {
                            Ok(email) => account.email = email,
                            Err(e) => failed = Some(e),
                        }
                    }
                }
                settings.write(&mut self.lock())?;
                failed.map_or(Ok(None), Err)
            }
            CalendarAction::Disconnect { id } => {
                let mut settings = ComposioSettings::try_read(&self.lock())?;
                if !settings.accounts.iter().any(|a| a.id == id) {
                    return Err("Unknown account.".into());
                }
                let key = self
                    .key()
                    .map_err(|_| "Save your Composio key first, so the account is disconnected at Composio too.".to_owned())?;
                delete_account(self.http.as_ref(), &key, &id)?;
                settings.accounts.retain(|a| a.id != id);
                settings.write(&mut self.lock())?;
                Ok(None)
            }
        }
    }
}
