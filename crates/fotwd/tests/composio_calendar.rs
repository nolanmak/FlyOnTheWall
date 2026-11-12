//! Google Calendar through Composio: the client, event parsing, the lookup
//! and the settings control, all against a fake Composio.
//!
//! Every event and account here is invented. This repo is public.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use fotw_secrets::{InMemoryKeyStore, KeyStore, Provider, SecretKey};
use fotw_store::{Db, DbKey};
use fotw_web::calendar::{AccountState, CalendarAction, CalendarControl};
use fotwd::calendar::composio::{
    self, Account, ComposioCalendar, ComposioHttp, ComposioSettings, Method, Request,
};
use fotwd::calendar::{CalendarLookup, Participation};
use serde_json::{Value, json};

const MIN: i64 = 60_000;
/// 2026-11-12T18:00:00Z
const T: i64 = 1_794_506_400_000;

/// Answers queued responses in order and records every request.
#[derive(Default)]
struct FakeComposio {
    replies: Mutex<VecDeque<(u16, Value)>>,
    seen: Mutex<Vec<(String, Request)>>,
}

impl FakeComposio {
    fn replying(replies: Vec<(u16, Value)>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            seen: Mutex::default(),
        })
    }
    fn requests(&self) -> Vec<Request> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r)| r.clone())
            .collect()
    }
    fn keys(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }
}

impl ComposioHttp for FakeComposio {
    fn send(&self, key: &str, request: &Request) -> Result<(u16, Value), String> {
        self.seen
            .lock()
            .unwrap()
            .push((key.to_owned(), request.clone()));
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| format!("unexpected request {request:?}"))
    }
}

fn google_event(id: &str, title: &str, start: &str, end: &str) -> Value {
    json!({
        "id": id,
        "summary": title,
        "status": "confirmed",
        "start": {"dateTime": start},
        "end": {"dateTime": end},
    })
}

// --- Dates -------------------------------------------------------------------

#[test]
fn formats_utc_rfc3339() {
    assert_eq!(composio::rfc3339_utc(T), "2026-11-12T18:00:00Z");
    assert_eq!(composio::rfc3339_utc(0), "1970-01-01T00:00:00Z");
}

#[test]
fn parses_rfc3339_with_offsets_and_fractions() {
    assert_eq!(composio::parse_rfc3339("2026-11-12T18:00:00Z"), Some(T));
    assert_eq!(
        composio::parse_rfc3339("2026-11-12T10:00:00-08:00"),
        Some(T)
    );
    assert_eq!(
        composio::parse_rfc3339("2026-11-12T23:30:00+05:30"),
        Some(T)
    );
    assert_eq!(
        composio::parse_rfc3339("2026-11-12T18:00:00.250Z"),
        Some(T + 250)
    );
    assert_eq!(composio::parse_rfc3339("2026-11-12"), None);
    assert_eq!(composio::parse_rfc3339("not a date"), None);
    assert_eq!(composio::parse_rfc3339("2026-13-40T99:00:00Z"), None);
}

// --- Parsing events ----------------------------------------------------------

#[test]
fn finds_events_wherever_composio_nests_them() {
    // The tool's `data` shape is not documented; events are found by shape.
    let data = json!({
        "calendars": [
            {"calendar_id": "primary", "events": [
                google_event("e1", "Design review", "2026-11-12T10:00:00-08:00", "2026-11-12T10:30:00-08:00"),
            ]},
            {"calendar_id": "team", "items": [
                google_event("e2", "Planning", "2026-11-12T19:00:00Z", "2026-11-12T19:30:00Z"),
            ]},
        ],
        "summary": {"total": 2},
    });
    let events = composio::events_from(&data);
    let titles: Vec<&str> = events.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, ["Design review", "Planning"]);
    assert_eq!(events[0].start_ms, T);
    assert_eq!(events[0].end_ms, T + 30 * MIN);
    assert_eq!(events[0].source, "composio");
}

#[test]
fn reads_all_day_cancelled_and_my_response() {
    let data = json!({"items": [
        {"id": "a", "summary": "Offsite", "start": {"date": "2026-11-12"}, "end": {"date": "2026-11-13"}},
        {"id": "b", "summary": "Dropped", "status": "cancelled",
         "start": {"dateTime": "2026-11-12T18:00:00Z"}, "end": {"dateTime": "2026-11-12T18:30:00Z"}},
        {"id": "c", "summary": "Optional talk",
         "start": {"dateTime": "2026-11-12T18:00:00Z"}, "end": {"dateTime": "2026-11-12T18:30:00Z"},
         "attendees": [{"email": "someone@example.test", "responseStatus": "accepted"},
                       {"email": "me@example.test", "self": true, "responseStatus": "declined"}]},
        {"id": "d", "summary": "Sync",
         "start": {"dateTime": "2026-11-12T18:00:00Z"}, "end": {"dateTime": "2026-11-12T18:30:00Z"},
         "attendees": [{"self": true, "responseStatus": "tentative"}]},
    ]});
    let events = composio::events_from(&data);
    assert_eq!(events.len(), 4);
    assert!(events[0].all_day);
    assert!(events[1].canceled);
    assert_eq!(events[2].participation, Participation::Declined);
    assert_eq!(events[3].participation, Participation::Tentative);
}

#[test]
fn the_same_event_from_two_calendars_is_kept_once() {
    let e = google_event(
        "dup",
        "Standup",
        "2026-11-12T18:00:00Z",
        "2026-11-12T18:15:00Z",
    );
    let data = json!({"a": {"items": [e.clone()]}, "b": {"items": [e]}});
    assert_eq!(composio::events_from(&data).len(), 1);
}

#[test]
fn objects_that_are_not_events_are_ignored() {
    let data = json!({"items": [
        {"id": "x", "summary": "No times"},
        {"start": {"dateTime": "garbage"}, "end": {"dateTime": "2026-11-12T18:00:00Z"}, "id": "y"},
        {"kind": "calendar#calendarListEntry", "id": "primary"},
    ]});
    assert!(composio::events_from(&data).is_empty());
}

// --- Client ------------------------------------------------------------------

#[test]
fn every_request_goes_to_composio_with_the_key() {
    let http = FakeComposio::replying(vec![(
        200,
        json!({"items": [{"id": "ac_1", "status": "ENABLED"}]}),
    )]);
    composio::ensure_auth_config(http.as_ref(), "key-1").unwrap();
    for request in http.requests() {
        assert!(
            fotwd::transport::is_allowed(&request.url),
            "{}",
            request.url
        );
        assert!(request.url.starts_with(composio::BASE));
    }
    assert_eq!(http.keys(), ["key-1"]);
}

#[test]
fn an_existing_auth_config_is_reused() {
    let http = FakeComposio::replying(vec![(
        200,
        json!({"items": [{"id": "ac_1", "status": "ENABLED"}]}),
    )]);
    assert_eq!(
        composio::ensure_auth_config(http.as_ref(), "k").unwrap(),
        "ac_1"
    );
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, Method::Get);
    assert!(requests[0].url.contains("toolkit_slug=googlecalendar"));
}

#[test]
fn a_missing_auth_config_is_created_with_composio_managed_auth() {
    let http = FakeComposio::replying(vec![
        (
            200,
            json!({"items": [{"id": "ac_old", "status": "DISABLED"}]}),
        ),
        (201, json!({"auth_config": {"id": "ac_new"}})),
    ]);
    assert_eq!(
        composio::ensure_auth_config(http.as_ref(), "k").unwrap(),
        "ac_new"
    );
    let create = &http.requests()[1];
    assert_eq!(create.method, Method::Post);
    let body = create.body.as_ref().unwrap();
    assert_eq!(body["toolkit"]["slug"], "googlecalendar");
    assert_eq!(body["auth_config"]["type"], "use_composio_managed_auth");
}

#[test]
fn a_link_returns_the_sign_in_page_and_account() {
    let http = FakeComposio::replying(vec![(
        201,
        json!({"redirect_url": "https://connect.composio.dev/link/x", "connected_account_id": "ca_9"}),
    )]);
    let link = composio::start_link(http.as_ref(), "k", "ac_1", "fotw-user").unwrap();
    assert_eq!(link.redirect_url, "https://connect.composio.dev/link/x");
    assert_eq!(link.account_id, "ca_9");
    let body = http.requests()[0].body.clone().unwrap();
    assert_eq!(body["auth_config_id"], "ac_1");
    assert_eq!(body["user_id"], "fotw-user");
}

#[test]
fn a_link_to_somewhere_other_than_https_is_refused() {
    let http = FakeComposio::replying(vec![(
        201,
        json!({"redirect_url": "javascript:alert(1)", "connected_account_id": "ca_9"}),
    )]);
    assert!(composio::start_link(http.as_ref(), "k", "ac_1", "u").is_err());
}

#[test]
fn account_status_maps_to_three_states() {
    for (raw, want) in [
        ("ACTIVE", AccountState::Active),
        ("INITIATED", AccountState::Pending),
        ("INITIALIZING", AccountState::Pending),
        ("EXPIRED", AccountState::Failed),
        ("FAILED", AccountState::Failed),
    ] {
        let http = FakeComposio::replying(vec![(200, json!({"id": "ca_1", "status": raw}))]);
        assert_eq!(
            composio::account_state(http.as_ref(), "k", "ca_1").unwrap(),
            want,
            "{raw}"
        );
    }
    let gone = FakeComposio::replying(vec![(404, json!({}))]);
    assert_eq!(
        composio::account_state(gone.as_ref(), "k", "ca_1").unwrap(),
        AccountState::Failed
    );
}

#[test]
fn a_server_error_on_status_is_one_request_and_an_error() {
    let http = FakeComposio::replying(vec![(500, json!({"message": "upstream down"}))]);
    let error = composio::account_state(http.as_ref(), "k", "ca_1").unwrap_err();
    assert!(
        error.contains("500") && error.contains("upstream down"),
        "{error}"
    );
    assert_eq!(http.requests().len(), 1);
}

#[test]
fn listing_events_asks_for_the_window_across_all_calendars() {
    let http = FakeComposio::replying(vec![(
        200,
        json!({"successful": true, "error": null, "data": {"items": [
            google_event("e1", "Design review", "2026-11-12T18:00:00Z", "2026-11-12T18:30:00Z")
        ]}}),
    )]);
    let events =
        composio::list_events(http.as_ref(), "k", "ca_1", T - 30 * MIN, T + 40 * MIN).unwrap();
    assert_eq!(events.len(), 1);
    let request = &http.requests()[0];
    assert_eq!(request.method, Method::Post);
    assert!(
        request
            .url
            .ends_with("/tools/execute/GOOGLECALENDAR_EVENTS_LIST_ALL_CALENDARS")
    );
    let body = request.body.as_ref().unwrap();
    assert_eq!(body["connected_account_id"], "ca_1");
    assert_eq!(body["arguments"]["time_min"], "2026-11-12T17:30:00Z");
    assert_eq!(body["arguments"]["time_max"], "2026-11-12T18:40:00Z");
    assert_eq!(body["arguments"]["response_detail"], "full");
    assert_eq!(body["arguments"]["single_events"], true);
}

#[test]
fn a_failed_tool_call_is_an_error() {
    let http = FakeComposio::replying(vec![(
        200,
        json!({"successful": false, "error": "token expired", "data": {}}),
    )]);
    let error = composio::list_events(http.as_ref(), "k", "ca_1", T, T + MIN).unwrap_err();
    assert!(error.contains("token expired"), "{error}");
}

#[test]
fn a_rejected_key_says_so() {
    let http = FakeComposio::replying(vec![(401, json!({"error": "bad key"}))]);
    let error = composio::list_events(http.as_ref(), "k", "ca_1", T, T + MIN).unwrap_err();
    assert!(error.contains("rejected"), "{error}");
}

// --- Lookup ------------------------------------------------------------------

fn account(id: &str, status: AccountState) -> Account {
    Account {
        id: id.into(),
        status,
        added_at_ms: 0,
    }
}

#[test]
fn the_lookup_reads_every_active_account_and_skips_the_rest() {
    let http = FakeComposio::replying(vec![
        (
            200,
            json!({"successful": true, "data": {"items": [google_event("e1", "Design review", "2026-11-12T18:00:00Z", "2026-11-12T18:30:00Z")]}}),
        ),
        (
            200,
            json!({"successful": true, "data": {"items": [google_event("e2", "Planning", "2026-11-12T18:00:00Z", "2026-11-12T18:30:00Z")]}}),
        ),
    ]);
    let lookup = ComposioCalendar::new(
        http.clone(),
        Arc::new(|| Ok(Some("k".to_owned()))),
        vec![
            account("ca_1", AccountState::Active),
            account("ca_2", AccountState::Pending),
            account("ca_3", AccountState::Active),
        ],
    );
    let events = lookup.events_overlapping(T, T + 30 * MIN).unwrap();
    assert_eq!(events.len(), 2);
    let mut asked: Vec<String> = http
        .requests()
        .iter()
        .map(|r| {
            r.body.as_ref().unwrap()["connected_account_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    asked.sort();
    assert_eq!(asked, ["ca_1", "ca_3"]);
}

#[test]
fn one_broken_account_does_not_hide_the_others() {
    let http = FakeComposio::replying(vec![
        (401, json!({})),
        (
            200,
            json!({"successful": true, "data": {"items": [google_event("e2", "Planning", "2026-11-12T18:00:00Z", "2026-11-12T18:30:00Z")]}}),
        ),
    ]);
    let lookup = ComposioCalendar::new(
        http,
        Arc::new(|| Ok(Some("k".to_owned()))),
        vec![
            account("ca_1", AccountState::Active),
            account("ca_2", AccountState::Active),
        ],
    );
    assert_eq!(lookup.events_overlapping(T, T + MIN).unwrap().len(), 1);
}

#[test]
fn every_account_failing_is_an_error_without_event_content() {
    let http = FakeComposio::replying(vec![(401, json!({}))]);
    let lookup = ComposioCalendar::new(
        http,
        Arc::new(|| Ok(Some("k".to_owned()))),
        vec![account("ca_1", AccountState::Active)],
    );
    assert!(lookup.events_overlapping(T, T + MIN).is_err());
}

#[test]
fn no_key_means_no_request() {
    let http = FakeComposio::replying(vec![]);
    let lookup = ComposioCalendar::new(
        http.clone(),
        Arc::new(|| Ok(None)),
        vec![account("ca_1", AccountState::Active)],
    );
    assert!(lookup.events_overlapping(T, T + MIN).unwrap().is_empty());
    assert!(http.requests().is_empty());
}

#[test]
fn a_composio_event_names_a_recording_through_the_shared_matcher() {
    let http = FakeComposio::replying(vec![(
        200,
        json!({"successful": true, "data": {"items": [google_event("e1", "Weekly 1:1", "2026-11-12T18:00:00Z", "2026-11-12T18:30:00Z")]}}),
    )]);
    let lookup: Arc<dyn CalendarLookup> = Arc::new(ComposioCalendar::new(
        http,
        Arc::new(|| Ok(Some("k".to_owned()))),
        vec![account("ca_1", AccountState::Active)],
    ));
    let link = fotwd::calendar::lookup_match(
        lookup,
        T + 2 * MIN,
        T + 27 * MIN,
        std::time::Duration::from_secs(2),
    )
    .unwrap()
    .unwrap();
    assert_eq!(link.title, "Weekly 1:1");
    assert_eq!(link.source, "composio");
}

#[test]
fn many_calendars_merge_and_keep_each_events_source() {
    let a: Arc<dyn CalendarLookup> = Arc::new(fotwd::calendar::Fixed::new(
        "eventkit",
        vec![fotwd::calendar::ScheduledEvent {
            uid: "x".into(),
            title: "From Apple".into(),
            start_ms: T,
            end_ms: T + 30 * MIN,
            all_day: false,
            canceled: false,
            participation: Participation::Unknown,
            source: "eventkit",
        }],
    ));
    let broken: Arc<dyn CalendarLookup> = Arc::new(Broken);
    let merged = fotwd::calendar::Many::new(vec![a, broken]);
    let events = merged.events_overlapping(T, T + MIN).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].source, "eventkit");
}

struct Broken;
impl CalendarLookup for Broken {
    fn source(&self) -> &'static str {
        "broken"
    }
    fn events_overlapping(
        &self,
        _: i64,
        _: i64,
    ) -> Result<Vec<fotwd::calendar::ScheduledEvent>, String> {
        Err("down".into())
    }
}

// --- Settings control --------------------------------------------------------

fn db() -> Db {
    Db::open_in_memory(&DbKey::from_bytes([7u8; 32])).unwrap()
}

fn control(
    http: Arc<FakeComposio>,
) -> (composio::CalendarSettingsControl, &'static InMemoryKeyStore) {
    let store: &'static InMemoryKeyStore = Box::leak(Box::new(InMemoryKeyStore::new()));
    (
        composio::CalendarSettingsControl::new(db(), store, http),
        store,
    )
}

fn saved_key(store: &InMemoryKeyStore) -> Option<String> {
    store
        .get(SecretKey::ApiKey(Provider::Composio))
        .ok()
        .map(|s| s.expose().to_owned())
}

#[test]
fn saving_a_key_checks_it_first() {
    let http = FakeComposio::replying(vec![(401, json!({}))]);
    let (c, store) = control(http);
    let error = c
        .act(CalendarAction::SaveKey {
            key: "bad-key".into(),
        })
        .unwrap_err();
    assert!(error.contains("rejected"), "{error}");
    assert_eq!(saved_key(store), None);
    assert!(!c.read().composio_key_saved);
}

#[test]
fn a_good_key_is_stored_and_never_read_back() {
    let http = FakeComposio::replying(vec![(200, json!({"items": []}))]);
    let (c, store) = control(http);
    c.act(CalendarAction::SaveKey {
        key: "  good-key  ".into(),
    })
    .unwrap();
    assert_eq!(saved_key(store).as_deref(), Some("good-key"));
    let shown = serde_json::to_string(&c.read()).unwrap();
    assert!(shown.contains("\"composio_key_saved\":true"));
    assert!(!shown.contains("good-key"));
}

#[test]
fn connecting_needs_a_key_and_the_acknowledgement() {
    let (c, _) = control(FakeComposio::replying(vec![]));
    let no_key = c
        .act(CalendarAction::Connect {
            acknowledge_egress: true,
        })
        .unwrap_err();
    assert!(no_key.to_lowercase().contains("key"), "{no_key}");

    let http = FakeComposio::replying(vec![(200, json!({"items": []}))]);
    let (c, _) = control(http);
    c.act(CalendarAction::SaveKey { key: "k".into() }).unwrap();
    let no_ack = c
        .act(CalendarAction::Connect {
            acknowledge_egress: false,
        })
        .unwrap_err();
    assert!(no_ack.contains("Composio"), "{no_ack}");
}

#[test]
fn connect_refresh_and_disconnect_round_trip() {
    let http = FakeComposio::replying(vec![
        (200, json!({"items": []})),                   // key check
        (200, json!({"items": []})),                   // find auth config
        (201, json!({"auth_config": {"id": "ac_1"}})), // create it
        (
            201,
            json!({"redirect_url": "https://connect.composio.dev/l/1", "connected_account_id": "ca_1"}),
        ),
        (
            201,
            json!({"redirect_url": "https://connect.composio.dev/l/2", "connected_account_id": "ca_2"}),
        ),
        (200, json!({"id": "ca_1", "status": "ACTIVE"})), // refresh ca_1
        (200, json!({"id": "ca_2", "status": "INITIATED"})), // refresh ca_2
        (200, json!({"success": true})),                  // delete ca_2
    ]);
    let (c, _) = control(http.clone());
    c.act(CalendarAction::SaveKey { key: "k".into() }).unwrap();

    let url = c
        .act(CalendarAction::Connect {
            acknowledge_egress: true,
        })
        .unwrap();
    assert_eq!(url.as_deref(), Some("https://connect.composio.dev/l/1"));
    // A second account reuses the saved auth config: no second lookup.
    let url = c
        .act(CalendarAction::Connect {
            acknowledge_egress: true,
        })
        .unwrap();
    assert_eq!(url.as_deref(), Some("https://connect.composio.dev/l/2"));
    let shown = c.read();
    assert!(shown.composio_acknowledged);
    assert_eq!(shown.google_accounts.len(), 2);
    assert!(
        shown
            .google_accounts
            .iter()
            .all(|a| a.status == AccountState::Pending)
    );

    c.act(CalendarAction::Refresh).unwrap();
    let states: Vec<AccountState> = c.read().google_accounts.iter().map(|a| a.status).collect();
    assert_eq!(states, [AccountState::Active, AccountState::Pending]);

    c.act(CalendarAction::Disconnect { id: "ca_2".into() })
        .unwrap();
    let left: Vec<String> = c
        .read()
        .google_accounts
        .iter()
        .map(|a| a.id.clone())
        .collect();
    assert_eq!(left, ["ca_1"]);
    let last = http.requests().pop().unwrap();
    assert_eq!(last.method, Method::Delete);
    assert!(
        last.url
            .ends_with("/connected_accounts/ca_2?revoke_on_delete=true")
    );
}

#[test]
fn forgetting_the_key_removes_it() {
    let http = FakeComposio::replying(vec![(200, json!({"items": []}))]);
    let (c, store) = control(http);
    c.act(CalendarAction::SaveKey { key: "k".into() }).unwrap();
    c.act(CalendarAction::ForgetKey).unwrap();
    assert_eq!(saved_key(store), None);
    assert!(!c.read().composio_key_saved);
}

#[test]
fn settings_round_trip_through_the_library() {
    let mut db = db();
    let saved = ComposioSettings {
        user_id: "fotw-abc".into(),
        auth_config_id: Some("ac_1".into()),
        acknowledged_egress: true,
        accounts: vec![account("ca_1", AccountState::Active)],
    };
    saved.write(&mut db).unwrap();
    assert_eq!(ComposioSettings::read(&db), saved);
    assert!(
        ComposioSettings::read(&Db::open_in_memory(&DbKey::from_bytes([1u8; 32])).unwrap())
            .accounts
            .is_empty()
    );
}

// --- Review fixes ------------------------------------------------------------

/// Answers after a long wait.
struct Slow;
impl CalendarLookup for Slow {
    fn source(&self) -> &'static str {
        "slow"
    }
    fn events_overlapping(
        &self,
        _: i64,
        _: i64,
    ) -> Result<Vec<fotwd::calendar::ScheduledEvent>, String> {
        std::thread::sleep(std::time::Duration::from_secs(30));
        Ok(Vec::new())
    }
}

#[test]
fn a_slow_google_account_does_not_cost_the_apple_answer() {
    let apple: Arc<dyn CalendarLookup> = Arc::new(fotwd::calendar::Fixed::new(
        "eventkit",
        vec![fotwd::calendar::ScheduledEvent {
            uid: "x".into(),
            title: "From Apple".into(),
            start_ms: T,
            end_ms: T + 30 * MIN,
            all_day: false,
            canceled: false,
            participation: Participation::Unknown,
            source: "eventkit",
        }],
    ));
    let merged = fotwd::calendar::Many::with_deadline(
        vec![apple, Arc::new(Slow)],
        std::time::Duration::from_millis(300),
    );
    let began = std::time::Instant::now();
    let events = merged.events_overlapping(T, T + MIN).unwrap();
    assert!(
        began.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].title, "From Apple");
}

#[test]
fn unreadable_settings_are_not_overwritten_by_an_action() {
    let mut db = db();
    db.put_setting(composio::SETTINGS_KEY, "{not json").unwrap();
    let store: &'static InMemoryKeyStore = Box::leak(Box::new(InMemoryKeyStore::new()));
    store
        .set(
            SecretKey::ApiKey(Provider::Composio),
            &fotw_secrets::SecretString::new("k"),
        )
        .unwrap();
    let c = composio::CalendarSettingsControl::new(db, store, FakeComposio::replying(vec![]));
    let error = c
        .act(CalendarAction::Connect {
            acknowledge_egress: true,
        })
        .unwrap_err();
    assert!(error.contains("could not be read"), "{error}");
    assert_eq!(c.stored_setting_for_tests().as_deref(), Some("{not json"));
}

#[test]
fn refresh_keeps_going_after_one_account_fails() {
    let mut db = db();
    ComposioSettings {
        user_id: "u".into(),
        auth_config_id: Some("ac".into()),
        acknowledged_egress: true,
        accounts: vec![
            account("ca_1", AccountState::Pending),
            account("ca_2", AccountState::Pending),
        ],
    }
    .write(&mut db)
    .unwrap();
    let store: &'static InMemoryKeyStore = Box::leak(Box::new(InMemoryKeyStore::new()));
    store
        .set(
            SecretKey::ApiKey(Provider::Composio),
            &fotw_secrets::SecretString::new("k"),
        )
        .unwrap();
    let http = FakeComposio::replying(vec![
        (500, json!({})),
        (200, json!({"id": "ca_2", "status": "ACTIVE"})),
    ]);
    let c = composio::CalendarSettingsControl::new(db, store, http);
    let result = c.act(CalendarAction::Refresh);
    let states: Vec<AccountState> = c.read().google_accounts.iter().map(|a| a.status).collect();
    assert_eq!(states, [AccountState::Pending, AccountState::Active]);
    assert!(result.is_err(), "the failure is still reported");
}

#[test]
fn disconnect_without_a_key_keeps_the_account_and_says_why() {
    let mut db = db();
    ComposioSettings {
        user_id: "u".into(),
        auth_config_id: Some("ac".into()),
        acknowledged_egress: true,
        accounts: vec![account("ca_1", AccountState::Active)],
    }
    .write(&mut db)
    .unwrap();
    let store: &'static InMemoryKeyStore = Box::leak(Box::new(InMemoryKeyStore::new()));
    let http = FakeComposio::replying(vec![]);
    let c = composio::CalendarSettingsControl::new(db, store, http.clone());
    let error = c
        .act(CalendarAction::Disconnect { id: "ca_1".into() })
        .unwrap_err();
    assert!(error.contains("key"), "{error}");
    assert_eq!(c.read().google_accounts.len(), 1);
    assert!(http.requests().is_empty());
}

#[test]
fn disconnect_revokes_the_google_grant() {
    let http = FakeComposio::replying(vec![(200, json!({"success": true}))]);
    composio::delete_account(http.as_ref(), "k", "ca_1").unwrap();
    assert!(
        http.requests()[0]
            .url
            .ends_with("/connected_accounts/ca_1?revoke_on_delete=true"),
        "{}",
        http.requests()[0].url
    );
}

#[test]
fn a_new_key_forgets_the_auth_config_but_keeps_accounts() {
    let mut db = db();
    ComposioSettings {
        user_id: "u".into(),
        auth_config_id: Some("ac_old_project".into()),
        acknowledged_egress: true,
        accounts: vec![account("ca_1", AccountState::Active)],
    }
    .write(&mut db)
    .unwrap();
    let store: &'static InMemoryKeyStore = Box::leak(Box::new(InMemoryKeyStore::new()));
    store
        .set(
            SecretKey::ApiKey(Provider::Composio),
            &fotw_secrets::SecretString::new("old"),
        )
        .unwrap();
    let http = FakeComposio::replying(vec![
        (200, json!({"items": []})),
        (200, json!({"items": []})),
    ]);
    let c = composio::CalendarSettingsControl::new(db, store, http);
    // Same key again: nothing to forget.
    c.act(CalendarAction::SaveKey { key: "old".into() })
        .unwrap();
    let kept: ComposioSettings =
        serde_json::from_str(&c.stored_setting_for_tests().unwrap()).unwrap();
    assert_eq!(kept.auth_config_id.as_deref(), Some("ac_old_project"));
    // A different key: the config may belong to another project.
    c.act(CalendarAction::SaveKey { key: "new".into() })
        .unwrap();
    let after: ComposioSettings =
        serde_json::from_str(&c.stored_setting_for_tests().unwrap()).unwrap();
    assert_eq!(after.auth_config_id, None);
    assert_eq!(
        after.accounts.len(),
        1,
        "accounts are kept across a key change"
    );
}

/// The first account never answers; the second does at once.
struct StallFirst;
impl ComposioHttp for StallFirst {
    fn send(&self, _key: &str, request: &Request) -> Result<(u16, Value), String> {
        let body = request.body.as_ref().unwrap();
        if body["connected_account_id"] == "ca_slow" {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
        Ok((
            200,
            json!({"successful": true, "data": {"items": [
                google_event("e2", "Planning", "2026-11-12T18:00:00Z", "2026-11-12T18:30:00Z")
            ]}}),
        ))
    }
}

#[test]
fn a_stalled_account_does_not_block_the_next_one() {
    let lookup = ComposioCalendar::new(
        Arc::new(StallFirst),
        Arc::new(|| Ok(Some("k".to_owned()))),
        vec![
            account("ca_slow", AccountState::Active),
            account("ca_fast", AccountState::Active),
        ],
    )
    .with_deadline(std::time::Duration::from_millis(300));
    let began = std::time::Instant::now();
    let events = lookup.events_overlapping(T, T + MIN).unwrap();
    assert!(
        began.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(events.len(), 1);
}
