//! Opt-in check of the Composio calendar path against the real API.
//!
//! Never runs in CI: it needs `FOTW_COMPOSIO_LIVE_KEY` (a project API key with
//! connected Google accounts) and is `#[ignore]`d. Optional:
//! `FOTW_COMPOSIO_LIVE_START_MS` / `FOTW_COMPOSIO_LIVE_END_MS` pick the
//! recording window to name (defaults to the last hour).
//!
//! ```sh
//! FOTW_COMPOSIO_LIVE_KEY=ak_... cargo test -p fotwd --test composio_live -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use fotw_web::calendar::{AccountState, GoogleAccount};
use fotwd::calendar::composio::{self, ComposioCalendar, ComposioHttp, ReqwestHttp};
use fotwd::calendar::{CalendarLookup, lookup_match};

#[test]
#[ignore = "talks to the real Composio API; needs FOTW_COMPOSIO_LIVE_KEY"]
fn names_a_recording_from_live_google_calendars() {
    let Ok(key) = std::env::var("FOTW_COMPOSIO_LIVE_KEY") else {
        eprintln!("FOTW_COMPOSIO_LIVE_KEY not set; skipping");
        return;
    };
    let now = fotw_store::now_ms();
    let env_ms = |name: &str, default: i64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let (start, end) = (
        env_ms("FOTW_COMPOSIO_LIVE_START_MS", now - 3_600_000),
        env_ms("FOTW_COMPOSIO_LIVE_END_MS", now),
    );

    // The connected accounts and the user they belong to, as Composio has them.
    let http: Arc<dyn ComposioHttp> = Arc::new(ReqwestHttp::default());
    let request = composio::Request {
        method: composio::Method::Get,
        url: format!(
            "{}/connected_accounts?toolkit_slugs=googlecalendar&statuses=ACTIVE&limit=50",
            composio::BASE
        ),
        body: None,
    };
    let (status, listed) = http.send(&key, &request).expect("reach Composio");
    assert_eq!(status, 200, "listing accounts: {listed}");
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert!(!items.is_empty(), "no active Google Calendar accounts");
    let user_id = items[0]["user_id"].as_str().unwrap_or_default().to_owned();
    let accounts: Vec<GoogleAccount> = items
        .iter()
        .filter_map(|i| i["id"].as_str())
        .map(|id| GoogleAccount {
            id: id.to_owned(),
            status: AccountState::Active,
            added_at_ms: 0,
            email: None,
        })
        .collect();

    for account in &accounts {
        let email = composio::primary_email(http.as_ref(), &key, &account.id, &user_id)
            .expect("list calendars");
        eprintln!(
            "account {} -> {:?}",
            &account.id[..8.min(account.id.len())],
            email
        );
    }

    let calendar = ComposioCalendar::new(
        Arc::clone(&http),
        Arc::new({
            let key = key.clone();
            move || Ok(Some(key.clone()))
        }),
        user_id,
        accounts,
    );
    let events = calendar
        .events_overlapping(start - 1_800_000, end + 600_000)
        .expect("read events");
    eprintln!("{} event(s) near the window", events.len());
    for e in &events {
        eprintln!(
            "  {:?} {}..{} all_day={}",
            e.title, e.start_ms, e.end_ms, e.all_day
        );
    }
    let lookup: Arc<dyn CalendarLookup> = Arc::new(calendar);
    let link = lookup_match(lookup, start, end, Duration::from_secs(20)).expect("lookup");
    eprintln!("named: {:?}", link.as_ref().map(|l| (&l.title, &l.source)));
}
