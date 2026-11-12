//! #295: a recording that overlaps a calendar event is saved under that
//! event's name, and enrichment leaves the name alone.
//!
//! Every event here is invented. This repo is public; never paste a real
//! calendar entry into a fixture.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fotw_secrets::InMemoryKeyStore;
use fotw_store::{Db, DbKey};
use fotw_stt::{Source, TimestampSource, TranscriptSegment, Word};
use fotwd::calendar::{
    CalendarLink, CalendarLookup, Fixed, LookupError, Participation, ScheduledEvent, Unavailable,
    clean_event_title, lookup_match,
};
use fotwd::enrich::enrich_meeting_with;
use fotwd::session::{LegBuffers, SessionOutcome};

const MIN: i64 = 60_000;
const T10: i64 = 1_800_000_000_000;

fn event(uid: &str, title: &str, start: i64, end: i64) -> ScheduledEvent {
    ScheduledEvent {
        uid: uid.into(),
        title: title.into(),
        start_ms: start,
        end_ms: end,
        all_day: false,
        canceled: false,
        participation: Participation::Accepted,
    }
}

/// A calendar that never answers in time.
struct Slow;
impl CalendarLookup for Slow {
    fn source(&self) -> &'static str {
        "slow"
    }
    fn events_overlapping(&self, _: i64, _: i64) -> Result<Vec<ScheduledEvent>, String> {
        std::thread::sleep(Duration::from_secs(30));
        Ok(vec![event("late", "Too late", T10, T10 + 30 * MIN)])
    }
}

/// A calendar that errors.
struct Failing;
impl CalendarLookup for Failing {
    fn source(&self) -> &'static str {
        "failing"
    }
    fn events_overlapping(&self, _: i64, _: i64) -> Result<Vec<ScheduledEvent>, String> {
        Err("store unavailable".into())
    }
}

/// Records the window it was asked about.
struct Spy(std::sync::Mutex<Option<(i64, i64)>>);
impl CalendarLookup for Spy {
    fn source(&self) -> &'static str {
        "spy"
    }
    fn events_overlapping(&self, s: i64, e: i64) -> Result<Vec<ScheduledEvent>, String> {
        *self.0.lock().unwrap() = Some((s, e));
        Ok(Vec::new())
    }
}

#[test]
fn lookup_returns_the_matching_event_as_a_link() {
    let calendar: Arc<dyn CalendarLookup> = Arc::new(Fixed::new(
        "test",
        vec![event("evt-1", "Weekly 1:1 — Product", T10, T10 + 30 * MIN)],
    ));
    let found = lookup_match(
        calendar,
        T10 + 2 * MIN,
        T10 + 27 * MIN,
        Duration::from_secs(2),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        found,
        CalendarLink {
            title: "Weekly 1:1 — Product".into(),
            uid: "evt-1".into(),
            source: "test".into(),
        }
    );
}

#[test]
fn lookup_asks_for_the_whole_tolerance_window() {
    let spy = Arc::new(Spy(std::sync::Mutex::new(None)));
    let calendar: Arc<dyn CalendarLookup> = spy.clone();
    let _ = lookup_match(calendar, T10, T10 + 30 * MIN, Duration::from_secs(2));
    // Events ending up to 30 min before the start, or starting up to 10 min
    // after the end, are candidates, so the query has to reach that far.
    assert_eq!(
        *spy.0.lock().unwrap(),
        Some((T10 - 30 * MIN, T10 + 40 * MIN))
    );
}

#[test]
fn no_calendar_means_no_match() {
    let calendar: Arc<dyn CalendarLookup> = Arc::new(Unavailable);
    assert_eq!(
        lookup_match(calendar, T10, T10 + MIN, Duration::from_secs(2)).unwrap(),
        None
    );
}

#[test]
fn a_hung_calendar_gives_up_within_the_timeout() {
    let calendar: Arc<dyn CalendarLookup> = Arc::new(Slow);
    let began = Instant::now();
    let result = lookup_match(calendar, T10, T10 + MIN, Duration::from_millis(200));
    assert!(matches!(result, Err(LookupError::TimedOut)), "{result:?}");
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
}

#[test]
fn a_failing_calendar_is_an_error_not_a_panic() {
    let calendar: Arc<dyn CalendarLookup> = Arc::new(Failing);
    let result = lookup_match(calendar, T10, T10 + MIN, Duration::from_secs(2));
    assert!(matches!(result, Err(LookupError::Failed(_))), "{result:?}");
}

#[test]
fn the_error_never_carries_an_event_title() {
    // The error is what gets logged; a title in it would put calendar content
    // in fotwd.log.
    let shown = LookupError::Failed("store unavailable".into()).to_string();
    assert!(!shown.contains("Weekly"));
    assert!(LookupError::TimedOut.to_string().contains("timed out"));
}

#[test]
fn event_titles_are_cleaned_like_any_stored_name() {
    assert_eq!(
        clean_event_title("  Design\treview  "),
        Some("Design review".into())
    );
    assert_eq!(
        clean_event_title("Line one\nLine two"),
        Some("Line one".into())
    );
    assert_eq!(
        clean_event_title("Bell\u{7}ringer"),
        Some("Bell ringer".into())
    );
    assert_eq!(clean_event_title(" \n\t "), None);
    let long = "word ".repeat(60);
    let cleaned = clean_event_title(&long).unwrap();
    assert!(cleaned.chars().count() <= fotw_web::documents::MAX_TITLE_CHARS);
    assert!(!cleaned.ends_with(' '));
}

#[test]
fn a_title_that_cleans_to_nothing_is_not_a_match() {
    let calendar: Arc<dyn CalendarLookup> = Arc::new(Fixed::new(
        "test",
        vec![event("evt-1", "\u{7}\u{8}", T10, T10 + 30 * MIN)],
    ));
    assert_eq!(
        lookup_match(calendar, T10, T10 + 30 * MIN, Duration::from_secs(2)).unwrap(),
        None
    );
}

// --- Persist -----------------------------------------------------------------

const PLACEHOLDER: &str = "Untitled recording — 2027-01-15 10:00 UTC";

fn library() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(
        dir.path().join("db.sqlite3"),
        &DbKey::from_bytes([0xcd; 32]),
    )
    .unwrap();
    (dir, db)
}

fn outcome(dir: &std::path::Path) -> SessionOutcome {
    let text = "so the plan for today is the pricing page";
    SessionOutcome {
        dir: dir.to_path_buf(),
        started_at_ms: 0,
        capture_ended_at_ms: None,
        system_samples: 480_000,
        mic_samples: 240_000,
        system_buffers: LegBuffers {
            silent: 2,
            total: 400,
        },
        mic_buffers: Some(LegBuffers {
            silent: 0,
            total: 400,
        }),
        dropped_samples: 0,
        segments: vec![TranscriptSegment {
            id: "seg-0".into(),
            session_id: "test-session".into(),
            source: Source::System,
            speaker: None,
            text: text.into(),
            start_ms: 0,
            end_ms: 4_000,
            words: vec![Word {
                text: "so".into(),
                start_ms: 0,
                end_ms: 200,
                confidence: Some(0.9),
                speaker: None,
            }],
            confidence: Some(0.9),
            language: Some("en".into()),
            is_final: true,
            revision: 0,
            provider: "deepgram".into(),
            model: "nova-3".into(),
            timestamp_source: TimestampSource::Provider,
        }],
        stt_errors: Vec::new(),
    }
}

fn link(title: &str) -> CalendarLink {
    CalendarLink {
        title: title.into(),
        uid: "evt-1".into(),
        source: "eventkit".into(),
    }
}

fn persisted(db: &mut Db, dir: &std::path::Path, link: Option<&CalendarLink>) -> String {
    fotwd::persist::persist_session_linked(db, &outcome(dir), PLACEHOLDER, link).unwrap()
}

fn calendar_columns(db: &Db, id: &str) -> (Option<String>, Option<String>) {
    db.conn()
        .query_row(
            "SELECT calendar_uid, calendar_source FROM meetings WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn persisting_with_a_link_names_the_meeting_and_records_the_event() {
    let (dir, mut db) = library();
    let id = persisted(&mut db, dir.path(), Some(&link("Weekly 1:1 — Product")));
    assert_eq!(
        db.meetings().get(&id).unwrap().title,
        "Weekly 1:1 — Product"
    );
    assert_eq!(
        calendar_columns(&db, &id),
        (Some("evt-1".into()), Some("eventkit".into()))
    );
}

#[test]
fn persisting_without_a_link_keeps_the_placeholder() {
    let (dir, mut db) = library();
    let id = persisted(&mut db, dir.path(), None);
    assert_eq!(db.meetings().get(&id).unwrap().title, PLACEHOLDER);
    assert_eq!(calendar_columns(&db, &id), (None, None));
}

#[test]
fn the_plain_persist_entry_point_still_links_nothing() {
    let (dir, mut db) = library();
    let id = fotwd::persist::persist_session(&mut db, &outcome(dir.path()), PLACEHOLDER).unwrap();
    assert_eq!(calendar_columns(&db, &id), (None, None));
}

// --- Enrichment --------------------------------------------------------------

#[tokio::test]
async fn enrichment_without_an_engine_keeps_the_calendar_title() {
    let (dir, mut db) = library();
    let id = persisted(&mut db, dir.path(), Some(&link("Design review")));
    let report = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &id).await;
    assert_eq!(db.meetings().get(&id).unwrap().title, "Design review");
    assert_eq!(report.title.as_deref(), Some("Design review"));
}

#[tokio::test]
async fn enrichment_without_a_calendar_still_names_from_the_transcript() {
    let (dir, mut db) = library();
    let id = persisted(&mut db, dir.path(), None);
    let _ = enrich_meeting_with(&mut db, &InMemoryKeyStore::new(), &id).await;
    assert_eq!(
        db.meetings().get(&id).unwrap().title,
        "so the plan for today is the pricing page"
    );
}

#[tokio::test]
async fn a_rename_of_a_calendar_title_survives_enrichment() {
    let (dir, mut db) = library();
    let id = persisted(&mut db, dir.path(), Some(&link("Design review")));
    let store = InMemoryKeyStore::new();
    let _ = enrich_meeting_with(&mut db, &store, &id).await;
    db.meetings()
        .set_title(&id, "Design review, round two")
        .unwrap();
    let _ = enrich_meeting_with(&mut db, &store, &id).await;
    assert_eq!(
        db.meetings().get(&id).unwrap().title,
        "Design review, round two"
    );
}

#[test]
fn the_legacy_sweep_never_adopts_a_calendar_title() {
    let (dir, mut db) = library();
    let id = persisted(&mut db, dir.path(), Some(&link("Design review")));
    let found = fotwd::enrich::adopt_legacy_titles(&mut db);
    assert!(!found.adopted.contains(&id));
}

#[test]
fn an_event_named_like_the_placeholder_is_not_used() {
    // Enrichment treats an `Untitled recording` prefix as replaceable, so a
    // calendar title wearing it would be silently overwritten. Better to fall
    // back to the usual naming from the start.
    let calendar: Arc<dyn CalendarLookup> = Arc::new(Fixed::new(
        "test",
        vec![event(
            "evt-1",
            "Untitled recording — notes",
            T10,
            T10 + 30 * MIN,
        )],
    ));
    assert_eq!(
        lookup_match(calendar, T10, T10 + 30 * MIN, Duration::from_secs(2)).unwrap(),
        None
    );
}
