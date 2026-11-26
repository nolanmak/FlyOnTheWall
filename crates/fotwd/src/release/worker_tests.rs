//! The release worker, state machine and control (#331): one test per row of
//! #326 5.3 and per settings effect in 5.1, plus the cross-cutting rules.
//!
//! Offline: a fake preparer, a fake `gws`, a fixed calendar, an injected
//! clock and an in-memory library. Synthetic data only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotw_web::release::{OutboxAction, ReleaseControl, ReleaseMode, ReleaseSettings};

use super::control::Control;
use super::settings::{self, SETTINGS_KEY};
use super::store::{self, ReleaseRow, decision, state};
use super::types::{
    BoxFuture, Bullet, Category, Cut, Finding, IndexMap, Payload, PrepareError, PrepareInput,
    Preparer, ReleaseSummary,
};
use super::worker::{
    CalendarSource, Clock, DELAYED_MS, Deps, PolicyCheck, PolicyError, Worker, fingerprint,
    load_input,
};
use super::{MailConfig, MailError, MailMessage, MailRunner, SendPermit};
use crate::calendar::{
    Attendee, AttendeeKind, CalendarLookup, Fixed, Participation, Response, ScheduledEvent,
};

const T0: i64 = 1_790_000_000_000;
const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
const ENGINE: &str = "claude-cli:fake-model";

// ------------------------------------------------------------------ fakes

/// Which step a fake preparation fails at, and how.
#[derive(Default)]
struct FakePreparer {
    cut: Mutex<Cut>,
    findings: Mutex<Vec<Finding>>,
    fail: Mutex<HashMap<&'static str, PrepareError>>,
    engine: Mutex<String>,
    calls: Mutex<Vec<&'static str>>,
    /// Runs inside the summary call, as a concurrent edit would.
    during_summary: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

impl FakePreparer {
    fn new() -> Self {
        let fake = Self::default();
        *fake.cut.lock().unwrap() = Cut {
            end_index: 99,
            omit_ranges: Vec::new(),
            reviewer_retained: None,
        };
        *fake.engine.lock().unwrap() = ENGINE.to_owned();
        fake
    }
    fn failing(&self, step: &'static str, error: PrepareError) {
        self.fail.lock().unwrap().insert(step, error);
    }
    fn heal(&self) {
        self.fail.lock().unwrap().clear();
    }
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
    fn step(&self, name: &'static str) -> Result<(), PrepareError> {
        self.calls.lock().unwrap().push(name);
        match self.fail.lock().unwrap().get(name) {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}

impl Preparer for FakePreparer {
    fn cut<'a>(&'a self, _: &'a PrepareInput) -> BoxFuture<'a, Result<Cut, PrepareError>> {
        Box::pin(async move {
            self.step("cut")?;
            Ok(self.cut.lock().unwrap().clone())
        })
    }
    fn summarize<'a>(
        &'a self,
        _: &'a PrepareInput,
        retained: &'a [usize],
    ) -> BoxFuture<'a, Result<ReleaseSummary, PrepareError>> {
        Box::pin(async move {
            if let Some(hook) = self.during_summary.lock().unwrap().as_mut() {
                hook();
            }
            self.step("summarize")?;
            Ok(ReleaseSummary {
                summary: vec![Bullet {
                    text: "The plan was agreed.".into(),
                    citations: vec![retained[0]],
                }],
                ..ReleaseSummary::default()
            })
        })
    }
    fn render(
        &self,
        input: &PrepareInput,
        retained: &[usize],
        _: &ReleaseSummary,
    ) -> Result<Payload, PrepareError> {
        self.step("render")?;
        let attachment = retained
            .iter()
            .map(|i| input.segments[*i].text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Payload {
            subject: format!("Notes: {} (2026-09-21)", input.event_title),
            body: "Summary\n- The plan was agreed.".into(),
            attachment_name: "2026-09-21-planning-sync.md".into(),
            attachment,
            index_map: IndexMap::default(),
        })
    }
    fn review<'a>(
        &'a self,
        _: &'a PrepareInput,
        _: &'a [usize],
        _: &'a Payload,
    ) -> BoxFuture<'a, Result<Vec<Finding>, PrepareError>> {
        Box::pin(async move {
            self.step("review")?;
            Ok(self.findings.lock().unwrap().clone())
        })
    }
    fn engine_id(&self) -> String {
        self.engine.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone)]
struct Sent {
    to: Vec<String>,
    subject: String,
}

/// A scripted `gws`.
#[derive(Default)]
struct FakeMail {
    answers: Mutex<Vec<Result<Option<String>, MailError>>>,
    sent: Mutex<Vec<Sent>>,
    profile: Mutex<Option<Result<String, MailError>>>,
    /// Runs while `gws` "is running", as a concurrent click would.
    during_send: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

impl FakeMail {
    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
    fn answer(&self, a: Result<Option<String>, MailError>) {
        self.answers.lock().unwrap().push(a);
    }
}

impl MailRunner for FakeMail {
    fn send(
        &self,
        _permit: SendPermit,
        _config: &MailConfig,
        m: &MailMessage<'_>,
    ) -> Result<Option<String>, MailError> {
        if let Some(hook) = self.during_send.lock().unwrap().as_mut() {
            hook();
        }
        self.sent.lock().unwrap().push(Sent {
            to: m.to.to_vec(),
            subject: m.subject.to_owned(),
        });
        let mut answers = self.answers.lock().unwrap();
        if answers.is_empty() {
            Ok(Some("msg-1".into()))
        } else {
            answers.remove(0)
        }
    }
    fn profile_address(&self, _: &MailConfig) -> Result<String, MailError> {
        self.profile
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Ok("owner@example.com".into()))
    }
}

struct FakePolicy(Mutex<Result<(), PolicyError>>);

impl PolicyCheck for FakePolicy {
    fn check(&self, _: &Db) -> Result<(), PolicyError> {
        *self.0.lock().unwrap()
    }
}

// ------------------------------------------------------------------ rig

struct Rig {
    db: Arc<Mutex<Db>>,
    now: Arc<AtomicI64>,
    preparer: Arc<FakePreparer>,
    mail: Arc<FakeMail>,
    policy: Arc<FakePolicy>,
    events: Arc<Mutex<Vec<ScheduledEvent>>>,
    worker: Worker,
    control: Control,
    receipts: Mutex<Vec<String>>,
    _lib: tempfile::TempDir,
}

fn person(address: &str, is_self: bool) -> Attendee {
    Attendee {
        address: Some(address.into()),
        display_name: Some(address.split('@').next().unwrap().into()),
        response: Response::Accepted,
        kind: AttendeeKind::Person,
        is_self,
        is_organizer: is_self,
    }
}

fn event(uid: &str, start: i64, attendees: Vec<Attendee>) -> ScheduledEvent {
    ScheduledEvent {
        uid: uid.into(),
        title: "Planning sync".into(),
        start_ms: start,
        end_ms: start + 30 * MIN,
        all_day: false,
        canceled: false,
        participation: Participation::Accepted,
        source: "eventkit",
        invitees: Vec::new(),
        ical_uid: Some(format!("ical-{uid}")),
        attendees,
    }
}

/// The five-attendee invite from the epic's first acceptance criterion.
fn five_attendees() -> Vec<Attendee> {
    let mut declined = person("cy@example.com", false);
    declined.response = Response::Declined;
    let mut room = person("room-1@example.com", false);
    room.kind = AttendeeKind::Resource;
    vec![
        person("ana@example.com", true),
        person("bo@example.com", false),
        declined,
        person("guest@example.org", false),
        room,
    ]
}

fn enabled(mode: ReleaseMode) -> ReleaseSettings {
    ReleaseSettings {
        enabled: true,
        mode,
        allowed_domains: vec!["example.com".into()],
        gws_profile_dir: "/profiles/example".into(),
        verified_sender: "owner@example.com".into(),
        release_since_ms: T0 - 60 * MIN,
        ..ReleaseSettings::default()
    }
}

fn rig() -> Rig {
    let db = Db::open_in_memory(&DbKey::from_bytes([9; 32])).unwrap();
    let db = Arc::new(Mutex::new(db));
    let now = Arc::new(AtomicI64::new(T0 + 24 * HOUR));
    let at = Arc::clone(&now);
    let clock: Clock = Arc::new(move || at.load(Ordering::SeqCst));
    let preparer = Arc::new(FakePreparer::new());
    let mail = Arc::new(FakeMail::default());
    let policy = Arc::new(FakePolicy(Mutex::new(Ok(()))));
    let events: Arc<Mutex<Vec<ScheduledEvent>>> = Arc::default();
    let shown = Arc::clone(&events);
    let calendar: CalendarSource = Arc::new(move || {
        let fixed: Arc<dyn CalendarLookup> =
            Arc::new(Fixed::new("eventkit", shown.lock().unwrap().clone()));
        (fixed, Duration::from_secs(2))
    });
    let lib = tempfile::tempdir().unwrap();
    let worker = Worker::new(Deps {
        db: Arc::clone(&db),
        library: lib.path().to_owned(),
        preparer: Arc::clone(&preparer) as Arc<dyn Preparer>,
        runner: Arc::clone(&mail) as Arc<dyn MailRunner>,
        calendar,
        policy: Arc::clone(&policy) as Arc<dyn PolicyCheck>,
        clock: Arc::clone(&clock),
    })
    .with_call_deadline(Duration::from_secs(5));
    let control = Control::new(
        Arc::clone(&db),
        Arc::clone(&mail) as Arc<dyn MailRunner>,
        Arc::clone(&policy) as Arc<dyn PolicyCheck>,
        Arc::clone(&preparer) as Arc<dyn Preparer>,
        clock,
        worker.pauses(),
    );
    let rig = Rig {
        db,
        now,
        preparer,
        mail,
        policy,
        events,
        worker,
        control,
        receipts: Mutex::new(Vec::new()),
        _lib: lib,
    };
    rig.store(&enabled(ReleaseMode::Approval));
    rig
}

impl Rig {
    fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn advance(&self, ms: i64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap()
    }
    fn store(&self, s: &ReleaseSettings) {
        settings::write(&self.lock(), s, 1).unwrap();
    }
    fn stored(&self) -> ReleaseSettings {
        settings::load(&self.lock()).in_force().unwrap()
    }

    /// A finished, enriched meeting of `n` segments with a matching event.
    fn meeting_at(&self, start: i64, n: usize, attendees: Vec<Attendee>) -> String {
        let id = {
            let mut db = self.lock();
            let id = db
                .meetings()
                .create(
                    NewMeeting::new("dev-1", "UTC")
                        .title("Recorded title")
                        .started_at_ms(start),
                )
                .unwrap();
            let tid = db
                .meetings()
                .create_transcript(&id, "deepgram", "nova-3", true)
                .unwrap();
            let segments: Vec<NewSegment> = (0..n)
                .map(|i| {
                    let i64i = i64::try_from(i).unwrap();
                    NewSegment::new(i64i, i64i * 1_000, i64i * 1_000 + 900, format!("line {i}"))
                })
                .collect();
            db.meetings().append_segments(&tid, &segments).unwrap();
            db.meetings().finish(&id, start + 30 * MIN).unwrap();
            db.meetings().set_state(&id, "ready").unwrap();
            db.conn()
                .execute(
                    "UPDATE meetings SET calendar_uid = ?1, calendar_source = 'eventkit' WHERE id = ?2",
                    [format!("evt-{id}"), id.clone()],
                )
                .unwrap();
            id
        };
        self.receipts.lock().unwrap().push(id.clone());
        let body: Vec<String> = self
            .receipts
            .lock()
            .unwrap()
            .iter()
            .map(|m| format!(r#""{m}":{{"started_at_ms":1,"finished_at_ms":2,"minted_title":""}}"#))
            .collect();
        self.lock()
            .put_setting(
                crate::enrich::RECEIPTS_KEY,
                &format!("{{{}}}", body.join(",")),
            )
            .unwrap();
        self.events
            .lock()
            .unwrap()
            .push(event(&format!("evt-{id}"), start, attendees));
        id
    }

    fn meeting(&self) -> String {
        self.meeting_at(T0, 10, five_attendees())
    }

    fn row(&self, id: &str) -> ReleaseRow {
        store::get(self.lock().conn(), id).unwrap().expect("a row")
    }
    fn state(&self, id: &str) -> (String, Vec<String>) {
        let row = self.row(id);
        (row.state, row.reasons)
    }

    async fn pass(&self) {
        self.worker.prepare_pass().await;
    }
    fn tick(&self) {
        self.worker.send_tick();
    }

    /// A meeting prepared into `needs_review`.
    async fn reviewed(&self) -> String {
        let id = self.meeting();
        self.pass().await;
        assert_eq!(self.row(&id).state, state::NEEDS_REVIEW);
        id
    }

    /// A meeting approved and `scheduled` for now.
    async fn scheduled(&self) -> String {
        let id = self.reviewed().await;
        self.control.act(&id, OutboxAction::Approve).unwrap();
        assert_eq!(self.row(&id).state, state::SCHEDULED);
        id
    }

    fn decisions(&self) -> Vec<String> {
        let db = self.lock();
        let mut stmt = db
            .conn()
            .prepare("SELECT decision FROM release_decisions ORDER BY at_ms, rowid")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn seed_decisions(&self, engine: &str, unchanged: usize, other: usize) {
        let db = self.lock();
        for _ in 0..unchanged {
            store::record_decision(db.conn(), "gone", engine, decision::APPROVED_UNCHANGED, 1)
                .unwrap();
        }
        for _ in 0..other {
            store::record_decision(db.conn(), "gone", engine, decision::DROPPED, 1).unwrap();
        }
    }

    fn know(&self, addresses: &[&str]) {
        let db = self.lock();
        for a in addresses {
            db.conn()
                .execute(
                    "INSERT INTO release_sent_addresses (address, first_sent_ms) VALUES (?1, 1)",
                    [a],
                )
                .unwrap();
        }
    }

    /// Auto mode, unlocked, with every recipient already known.
    fn auto_ready(&self) {
        self.seed_decisions(ENGINE, 20, 0);
        self.know(&["ana@example.com", "bo@example.com"]);
        self.store(&enabled(ReleaseMode::Auto));
    }

    fn reasons(&self, id: &str) -> Vec<String> {
        self.row(id).reasons
    }
}

fn has(reasons: &[String], r: &str) -> bool {
    reasons.iter().any(|x| x == r)
}

// ============================================================ 5.3, row by row

#[tokio::test]
async fn row_none_to_preparing_freezes_exactly_the_allowed_recipients() {
    let r = rig();
    r.preparer.failing("cut", PrepareError::SettingsChanged); // stay in preparing
    let id = r.meeting();
    r.pass().await;
    let row = r.row(&id);
    assert_eq!(row.state, state::PREPARING);
    let addresses: Vec<&str> = row.recipients.iter().map(|x| x.address.as_str()).collect();
    assert_eq!(addresses, ["ana@example.com", "bo@example.com"]);
    assert!(row.recipients[0].is_self);
    assert_eq!(row.event_title, "Planning sync");
    assert_eq!(row.source, "eventkit");
    assert_eq!(row.source_event_id, format!("evt-{id}"));
    assert_eq!(
        row.ical_uid.as_deref(),
        Some(format!("ical-evt-{id}").as_str())
    );
    // Frozen: a later calendar change does not touch the row (D4, R6).
    r.events.lock().unwrap()[0]
        .attendees
        .push(person("new@example.com", false));
    r.pass().await;
    assert_eq!(r.row(&id).recipients.len(), 2);
}

#[tokio::test]
async fn row_preparing_to_needs_review_in_approval_mode() {
    let r = rig();
    let id = r.meeting();
    r.pass().await;
    let row = r.row(&id);
    assert_eq!(row.state, state::NEEDS_REVIEW);
    assert!(has(&row.reasons, "review_required"));
    assert!(has(&row.reasons, "first_time_recipient"));
    assert_eq!(r.preparer.calls(), ["cut", "summarize", "render", "review"]);
    assert_eq!(
        row.subject.as_deref(),
        Some("Notes: Planning sync (2026-09-21)")
    );
    assert_eq!(
        row.attachment_name.as_deref(),
        Some("2026-09-21-planning-sync.md")
    );
    assert!(row.summary.is_some() && row.cut.is_some() && row.input_fingerprint.is_some());
    assert_eq!(row.engine_id.as_deref(), Some(ENGINE));
    assert_eq!(row.approved_by, None);
    assert!(
        r.decisions().is_empty(),
        "nothing is decided until a person acts"
    );
}

#[tokio::test]
async fn row_preparing_to_scheduled_in_unlocked_auto_mode() {
    let r = rig();
    r.auto_ready();
    let id = r.meeting();
    r.pass().await;
    let row = r.row(&id);
    assert_eq!(row.state, state::SCHEDULED, "{:?}", row.reasons);
    assert_eq!(row.approved_by.as_deref(), Some("auto"));
    assert_eq!(row.send_at, Some(r.now() + 15 * MIN));
    assert_eq!(
        r.decisions(),
        [decision::APPROVED_UNCHANGED; 20]
            .iter()
            .map(|d| (*d).to_owned())
            .chain(["auto".to_owned()])
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn auto_mode_holds_back_findings_omissions_and_large_audiences() {
    let r = rig();
    r.auto_ready();
    r.preparer.findings.lock().unwrap().push(Finding {
        category: Category::Compensation,
        indices: vec![1],
        bullet: None,
        reason: "a rate".into(),
    });
    let id = r.meeting();
    r.pass().await;
    assert_eq!(
        r.state(&id),
        (state::NEEDS_REVIEW.into(), vec!["has_findings".into()])
    );

    let r = rig();
    r.auto_ready();
    *r.preparer.cut.lock().unwrap() = Cut {
        end_index: 1,
        omit_ranges: Vec::new(),
        reviewer_retained: None,
    };
    let id = r.meeting();
    r.pass().await;
    assert_eq!(
        r.state(&id),
        (state::NEEDS_REVIEW.into(), vec!["mostly_omitted".into()])
    );

    let r = rig();
    r.auto_ready();
    let crowd: Vec<Attendee> = (0..51)
        .map(|i| person(&format!("p{i}@example.com"), i == 0))
        .collect();
    let addresses: Vec<String> = (0..51).map(|i| format!("p{i}@example.com")).collect();
    r.know(&addresses.iter().map(String::as_str).collect::<Vec<_>>());
    let id = r.meeting_at(T0, 10, crowd);
    r.pass().await;
    assert_eq!(
        r.state(&id),
        (state::NEEDS_REVIEW.into(), vec!["many_recipients".into()])
    );
}

#[tokio::test]
async fn row_preparing_to_held_on_a_failed_call_or_an_oversized_body() {
    let r = rig();
    r.preparer.failing("summarize", PrepareError::ModelFailed);
    let id = r.meeting();
    r.pass().await;
    assert_eq!(
        r.state(&id),
        (state::HELD.into(), vec!["model_failed".into()])
    );

    let r = rig();
    r.preparer.failing("render", PrepareError::BodyTooLarge);
    let id = r.meeting();
    r.pass().await;
    assert_eq!(
        r.state(&id),
        (state::HELD.into(), vec!["body_too_large".into()])
    );
    assert!(
        r.row(&id).cut.is_some(),
        "the cut is kept so Edit cuts can show it"
    );
}

#[tokio::test]
async fn row_needs_review_edit_cuts_reprepares_from_the_summary_step() {
    let r = rig();
    r.auto_ready(); // even unlocked auto never sends an edit
    r.preparer.findings.lock().unwrap().push(Finding {
        category: Category::OffMeeting,
        indices: vec![9],
        bullet: None,
        reason: "after the meeting".into(),
    });
    let id = r.meeting();
    r.pass().await;
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);
    r.preparer.findings.lock().unwrap().clear();
    r.preparer.calls.lock().unwrap().clear();

    let detail = r
        .control
        .act(
            &id,
            OutboxAction::EditCuts {
                retained: vec![0, 1, 2, 3, 4, 5, 6, 7, 8],
            },
        )
        .unwrap();
    assert_eq!(detail.item.state, state::PREPARING);
    assert_eq!(r.reasons(&id), ["edited"]);
    r.pass().await;
    assert_eq!(
        r.preparer.calls(),
        ["summarize", "render", "review"],
        "no new cut"
    );
    let row = r.row(&id);
    assert_eq!(row.state, state::NEEDS_REVIEW);
    assert_eq!(row.reasons, ["edited"]);
    assert_eq!(row.cut.unwrap().retained(10), (0..9).collect::<Vec<_>>());
    assert!(!row.attachment.unwrap().contains("line 9"));

    // An empty or out-of-range set is refused and changes nothing.
    assert!(
        r.control
            .act(&id, OutboxAction::EditCuts { retained: vec![] })
            .is_err()
    );
    assert!(
        r.control
            .act(&id, OutboxAction::EditCuts { retained: vec![10] })
            .is_err()
    );
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);
}

#[tokio::test]
async fn row_needs_review_approve_schedules_now_and_records_the_decision() {
    let r = rig();
    let id = r.reviewed().await;
    r.advance(3 * MIN);
    r.control.act(&id, OutboxAction::Approve).unwrap();
    let row = r.row(&id);
    assert_eq!(row.state, state::SCHEDULED);
    assert_eq!(row.send_at, Some(r.now()));
    assert_eq!(row.approved_by.as_deref(), Some("human"));
    assert!(row.reasons.is_empty());
    assert_eq!(r.decisions(), [decision::APPROVED_UNCHANGED]);

    // After Edit cuts, approval is `approved_edited`.
    let id = {
        let id = r.meeting_at(T0 + 5 * HOUR, 10, five_attendees());
        r.pass().await;
        id
    };
    r.control
        .act(
            &id,
            OutboxAction::EditCuts {
                retained: vec![0, 1, 2, 3, 4, 5, 6, 7],
            },
        )
        .unwrap();
    r.pass().await;
    r.control.act(&id, OutboxAction::Approve).unwrap();
    assert_eq!(
        r.decisions(),
        [decision::APPROVED_UNCHANGED, decision::APPROVED_EDITED]
    );
}

#[tokio::test]
async fn row_outcome_unknown_only_sends_again_with_its_own_decision() {
    let r = rig();
    let id = r.scheduled().await;
    r.mail.answer(Err(MailError::Timeout));
    r.tick();
    assert_eq!(
        r.state(&id),
        (
            state::NEEDS_REVIEW.into(),
            vec!["send_outcome_unknown".into()]
        )
    );
    assert!(r.control.act(&id, OutboxAction::Approve).is_err());
    assert!(
        r.control
            .act(&id, OutboxAction::EditCuts { retained: vec![0] })
            .is_err()
    );
    r.control.act(&id, OutboxAction::SendAgain).unwrap();
    let row = r.row(&id);
    assert_eq!(row.state, state::SCHEDULED);
    assert_eq!(row.approved_by.as_deref(), Some("human"));
    assert_eq!(
        r.decisions(),
        [decision::APPROVED_UNCHANGED, decision::SENT_AGAIN]
    );
    // Send again is only for that reason.
    let other = r.reviewed_second().await;
    assert!(r.control.act(&other, OutboxAction::SendAgain).is_err());
}

impl Rig {
    async fn reviewed_second(&self) -> String {
        let id = self.meeting_at(T0 + 7 * HOUR, 10, five_attendees());
        self.pass().await;
        id
    }
}

#[tokio::test]
async fn row_needs_review_drop_is_final_and_recorded() {
    let r = rig();
    let id = r.reviewed().await;
    r.control.act(&id, OutboxAction::Drop).unwrap();
    assert_eq!(
        r.state(&id),
        (state::DROPPED.into(), vec!["by_user".into()])
    );
    assert_eq!(r.decisions(), [decision::DROPPED]);
    assert!(r.control.act(&id, OutboxAction::Approve).is_err());
}

#[tokio::test]
async fn row_fingerprint_change_sends_review_scheduled_and_held_back_through_preparing() {
    let r = rig();
    let reviewed = r.reviewed().await;
    let scheduled = r.meeting_at(T0 + 2 * HOUR, 10, five_attendees());
    r.pass().await;
    r.control.act(&scheduled, OutboxAction::Approve).unwrap();
    r.preparer.failing("review", PrepareError::ModelFailed);
    let held = r.meeting_at(T0 + 4 * HOUR, 10, five_attendees());
    r.pass().await;
    assert_eq!(r.row(&held).state, state::HELD);
    r.preparer.heal();

    for id in [&reviewed, &scheduled, &held] {
        let db = r.lock();
        db.conn()
            .execute(
                "UPDATE segments SET text = 'changed words' WHERE meeting_id = ?1 AND idx = 0",
                [id],
            )
            .unwrap();
    }
    r.preparer.failing("cut", PrepareError::SettingsChanged); // observe `preparing`
    r.pass().await;
    for id in [&reviewed, &scheduled, &held] {
        assert_eq!(
            r.state(id),
            (state::PREPARING.into(), vec!["inputs_changed".into()])
        );
        assert_eq!(r.row(id).send_at, None);
    }
    r.preparer.heal();
    r.pass().await;
    for id in [&reviewed, &scheduled, &held] {
        let reasons = r.reasons(id);
        assert_eq!(r.row(id).state, state::NEEDS_REVIEW);
        assert!(has(&reasons, "inputs_changed"), "{reasons:?}");
    }
}

#[tokio::test]
async fn an_outcome_unknown_row_stays_put_when_its_inputs_change() {
    let r = rig();
    let id = r.scheduled().await;
    r.mail.answer(Err(MailError::Timeout));
    r.tick();
    r.lock()
        .conn()
        .execute(
            "UPDATE segments SET text = 'changed' WHERE meeting_id = ?1",
            [&id],
        )
        .unwrap();
    r.pass().await;
    assert_eq!(r.reasons(&id), ["send_outcome_unknown"]);
}

#[tokio::test]
async fn row_held_body_too_large_edit_cuts_then_needs_review_edited() {
    let r = rig();
    r.preparer.failing("render", PrepareError::BodyTooLarge);
    let id = r.meeting();
    r.pass().await;
    r.preparer.heal();
    r.control
        .act(
            &id,
            OutboxAction::EditCuts {
                retained: vec![0, 1, 2, 3],
            },
        )
        .unwrap();
    assert_eq!(
        r.state(&id),
        (state::PREPARING.into(), vec!["edited".into()])
    );
    r.pass().await;
    let reasons = r.reasons(&id);
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);
    assert!(has(&reasons, "edited"));
    // A model_failed row cannot be edited.
    let r = rig();
    r.preparer.failing("cut", PrepareError::ModelFailed);
    let id = r.meeting();
    r.pass().await;
    assert!(
        r.control
            .act(&id, OutboxAction::EditCuts { retained: vec![0] })
            .is_err()
    );
}

#[tokio::test]
async fn row_scheduled_cancel_returns_to_review_and_records_nothing() {
    let r = rig();
    r.auto_ready();
    let id = r.meeting();
    r.pass().await;
    assert_eq!(r.row(&id).state, state::SCHEDULED);
    let before = r.decisions().len();
    r.control.act(&id, OutboxAction::Cancel).unwrap();
    let row = r.row(&id);
    assert_eq!(row.state, state::NEEDS_REVIEW);
    assert_eq!(row.reasons, ["review_required"]);
    assert_eq!(row.send_at, None);
    assert_eq!(r.decisions().len(), before, "Cancel records no decision");
    r.advance(30 * MIN);
    r.tick();
    assert!(
        r.mail.sent().is_empty(),
        "a cancelled auto release never sends"
    );
}

#[tokio::test]
async fn row_scheduled_send_at_reached_sends_once_to_the_frozen_list() {
    let r = rig();
    let id = r.scheduled().await;
    r.tick();
    let sent = r.mail.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, ["ana@example.com", "bo@example.com"]);
    assert_eq!(sent[0].subject, "Notes: Planning sync (2026-09-21)");
    let row = r.row(&id);
    assert_eq!(row.state, state::SENT);
    assert_eq!(
        row.sent_to.as_deref(),
        Some(&["ana@example.com".to_owned(), "bo@example.com".to_owned()][..])
    );
    assert_eq!(row.message_id.as_deref(), Some("msg-1"));
    assert_eq!(row.sent_at, Some(r.now()));
    // The ledger learned both addresses (D19).
    assert!(
        store::all_known(
            r.lock().conn(),
            &["ana@example.com".into(), "bo@example.com".into()]
        )
        .unwrap()
    );
    r.advance(MIN);
    r.tick();
    assert_eq!(r.mail.sent().len(), 1, "sent is final (N2)");
    let status = r.control.meeting_status(&id);
    assert_eq!((status.state.as_str(), status.recipient_count), ("sent", 2));
}

#[tokio::test]
async fn row_sending_outcomes_map_per_d13() {
    // spawn failed → back to scheduled, worker paused gws_unavailable.
    let r = rig();
    let id = r.scheduled().await;
    r.mail.answer(Err(MailError::SpawnFailed));
    r.tick();
    assert_eq!(r.row(&id).state, state::SCHEDULED);
    assert_eq!(
        r.control.outbox().paused.as_deref(),
        Some("gws_unavailable")
    );
    r.tick();
    assert_eq!(r.row(&id).state, state::SENT, "resumes on its own");
    assert_eq!(r.control.outbox().paused, None);

    // A recognised rejection → held(send_refused).
    let r = rig();
    let id = r.scheduled().await;
    r.mail.answer(Err(MailError::Refused { http_status: 400 }));
    r.tick();
    assert_eq!(
        r.state(&id),
        (state::HELD.into(), vec!["send_refused".into()])
    );

    // Everything else → needs_review(send_outcome_unknown).
    let r = rig();
    let id = r.scheduled().await;
    r.mail.answer(Err(MailError::Other { code: "exit_1" }));
    r.tick();
    assert_eq!(
        r.state(&id),
        (
            state::NEEDS_REVIEW.into(),
            vec!["send_outcome_unknown".into()]
        )
    );
}

#[tokio::test]
async fn row_held_retry_reprepares_or_resends_the_same_snapshot() {
    let r = rig();
    r.preparer.failing("cut", PrepareError::ModelFailed);
    let id = r.meeting();
    r.pass().await;
    r.preparer.heal();
    r.control.act(&id, OutboxAction::Retry).unwrap();
    assert_eq!(r.row(&id).state, state::PREPARING);
    r.pass().await;
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);

    let r = rig();
    let id = r.scheduled().await;
    r.mail.answer(Err(MailError::Refused { http_status: 400 }));
    r.tick();
    let before = r.row(&id);
    r.advance(5 * MIN);
    r.control.act(&id, OutboxAction::Retry).unwrap();
    let after = r.row(&id);
    assert_eq!(after.state, state::SCHEDULED);
    assert_eq!(after.send_at, Some(r.now()));
    assert_eq!(after.subject, before.subject);
    assert_eq!(after.attachment, before.attachment);
    r.tick();
    assert_eq!(r.row(&id).state, state::SENT);

    // Retry checks the fingerprint first (D14).
    let r = rig();
    let id = r.scheduled().await;
    r.mail.answer(Err(MailError::Refused { http_status: 400 }));
    r.tick();
    r.lock()
        .conn()
        .execute(
            "UPDATE segments SET text = 'changed' WHERE meeting_id = ?1 AND idx = 1",
            [&id],
        )
        .unwrap();
    r.control.act(&id, OutboxAction::Retry).unwrap();
    assert_eq!(
        r.state(&id),
        (state::PREPARING.into(), vec!["inputs_changed".into()])
    );
}

#[tokio::test]
async fn row_held_drop_is_final_and_recorded() {
    let r = rig();
    r.preparer.failing("review", PrepareError::ModelFailed);
    let id = r.meeting();
    r.pass().await;
    r.control.act(&id, OutboxAction::Drop).unwrap();
    assert_eq!(
        r.state(&id),
        (state::DROPPED.into(), vec!["by_user".into()])
    );
    assert_eq!(r.decisions(), [decision::DROPPED]);
}

#[tokio::test]
async fn row_any_meeting_deleted_removes_the_row_but_not_its_decisions() {
    let r = rig();
    let id = r.reviewed().await;
    r.control.act(&id, OutboxAction::Drop).unwrap();
    r.seed_decisions(ENGINE, 20, 0);
    let unlocked = r.control.settings().auto_unlocked;
    r.lock().delete_meeting(&id).unwrap();
    assert!(store::get(r.lock().conn(), &id).unwrap().is_none());
    assert_eq!(r.decisions().len(), 21);
    assert_eq!(
        r.control.settings().auto_unlocked,
        unlocked,
        "deletion moves no evidence"
    );
}

// ============================================================ 5.1 effects

#[tokio::test]
async fn effect_disable_stops_creating_preparing_and_sending_and_keeps_rows() {
    let r = rig();
    let scheduled = r.scheduled().await;
    let preparing = r.meeting_at(T0 + 3 * HOUR, 10, five_attendees());
    r.preparer.failing("cut", PrepareError::SettingsChanged);
    r.pass().await;
    r.preparer.heal();
    let mut off = r.stored();
    off.enabled = false;
    let response = r.control.save_settings(off);
    assert_eq!(response.error, None);
    let fresh = r.meeting_at(T0 + 6 * HOUR, 10, five_attendees());
    r.pass().await;
    r.tick();
    assert_eq!(r.row(&scheduled).state, state::SCHEDULED);
    assert_eq!(r.row(&preparing).state, state::PREPARING);
    assert!(store::get(r.lock().conn(), &fresh).unwrap().is_none());
    assert!(r.mail.sent().is_empty());
    assert_eq!(r.control.meeting_status(&fresh).reasons, ["release_off"]);
}

#[tokio::test]
async fn effect_reenable_returns_scheduled_rows_and_restamps_the_cutoff() {
    let r = rig();
    let id = r.scheduled().await;
    let mut off = r.stored();
    off.enabled = false;
    let _ = r.control.save_settings(off.clone());
    r.advance(10 * MIN);
    let mut on = off;
    on.enabled = true;
    on.release_since_ms = 1; // a client stamp is ignored
    let response = r.control.save_settings(on);
    assert_eq!(response.error, None);
    assert_eq!(response.settings.release_since_ms, r.now());
    assert_eq!(
        r.state(&id),
        (
            state::NEEDS_REVIEW.into(),
            vec!["reenabled_while_scheduled".into()]
        )
    );
    r.tick();
    assert!(r.mail.sent().is_empty());
    // A meeting from before the new cutoff is never eligible (N4).
    let old = r.meeting_at(T0 + 20 * HOUR, 10, five_attendees());
    r.pass().await;
    assert!(store::get(r.lock().conn(), &old).unwrap().is_none());
    assert_eq!(r.control.meeting_status(&old).reasons, ["before_cutoff"]);
}

#[tokio::test]
async fn effect_auto_to_approval_returns_auto_rows_only() {
    let r = rig();
    r.auto_ready();
    let auto = r.meeting();
    r.pass().await;
    let human = r.meeting_at(T0 + 2 * HOUR, 10, five_attendees());
    r.preparer.findings.lock().unwrap().push(Finding {
        category: Category::Credential,
        indices: vec![0],
        bullet: None,
        reason: "a code".into(),
    });
    r.pass().await;
    r.control.act(&human, OutboxAction::Approve).unwrap();
    r.advance(-MIN); // keep `human` from being due before the switch
    let mut approval = r.stored();
    approval.mode = ReleaseMode::Approval;
    assert_eq!(r.control.save_settings(approval).error, None);
    assert_eq!(
        r.state(&auto),
        (state::NEEDS_REVIEW.into(), vec!["mode_changed".into()])
    );
    assert_eq!(r.row(&human).state, state::SCHEDULED);
}

#[tokio::test]
async fn effect_domain_removed_refilters_at_send_and_for_new_rows() {
    let r = rig();
    let mut attendees = five_attendees();
    attendees.push(person("dee@example.net", false));
    let mut two = enabled(ReleaseMode::Approval);
    two.allowed_domains.push("example.net".into());
    r.store(&two);
    let id = r.meeting_at(T0, 10, attendees);
    r.pass().await;
    assert_eq!(r.row(&id).recipients.len(), 3);
    r.control.act(&id, OutboxAction::Approve).unwrap();
    r.store(&enabled(ReleaseMode::Approval)); // example.net removed
    r.tick();
    assert_eq!(r.mail.sent()[0].to, ["ana@example.com", "bo@example.com"]);
    assert_eq!(r.row(&id).sent_to.unwrap().len(), 2);

    // Every recipient filtered out → dropped(no_recipients), nothing sent.
    let r = rig();
    let id = r.scheduled().await;
    let mut other = enabled(ReleaseMode::Approval);
    other.allowed_domains = vec!["example.net".into()];
    other.verified_sender = "owner@example.net".into();
    r.store(&other);
    *r.mail.profile.lock().unwrap() = Some(Ok("owner@example.net".into()));
    r.tick();
    assert_eq!(
        r.state(&id),
        (state::DROPPED.into(), vec!["no_recipients".into()])
    );
    assert!(r.mail.sent().is_empty());
    // New rows see the current domains: no recipient left.
    let fresh = r.meeting_at(T0 + 9 * HOUR, 10, five_attendees());
    r.pass().await;
    assert!(store::get(r.lock().conn(), &fresh).unwrap().is_none());
    assert_eq!(r.control.meeting_status(&fresh).reasons, ["no_recipients"]);
}

// ============================================================ settings

#[tokio::test]
async fn damaged_settings_pause_everything_and_never_take_defaults() {
    let r = rig();
    let id = r.scheduled().await;
    let pending = r.meeting_at(T0 + 3 * HOUR, 10, five_attendees());
    settings::put_meta(r.lock().conn(), SETTINGS_KEY, "{not json", 1).unwrap();
    r.pass().await;
    r.tick();
    assert!(r.mail.sent().is_empty());
    assert_eq!(
        r.row(&id).state,
        state::SCHEDULED,
        "no row changes while paused"
    );
    assert!(store::get(r.lock().conn(), &pending).unwrap().is_none());
    let response = r.control.settings();
    assert_eq!(response.paused.as_deref(), Some("settings_unreadable"));
    assert!(response.error.is_some());
    assert_eq!(
        r.control.outbox().paused.as_deref(),
        Some("settings_unreadable")
    );

    // An invalid replacement is refused; a complete valid one repairs it.
    let mut bad = enabled(ReleaseMode::Approval);
    bad.allowed_domains = vec!["*.example.com".into()];
    assert!(r.control.save_settings(bad).error.is_some());
    assert_eq!(settings::load(&r.lock()), settings::Stored::Damaged);
    let fixed = r.control.save_settings(enabled(ReleaseMode::Approval));
    assert_eq!(fixed.error, None);
    assert_eq!(fixed.paused, None);
    // Repairing counts as enabling: the cutoff is new and scheduled rows wait.
    assert_eq!(fixed.settings.release_since_ms, r.now());
    assert_eq!(r.reasons(&id), ["reenabled_while_scheduled"]);
}

#[tokio::test]
async fn enabling_needs_domains_a_passing_preflight_and_the_policy() {
    let r = rig();
    let mut off = enabled(ReleaseMode::Approval);
    off.enabled = false;
    off.verified_sender.clear();
    r.store(&off);

    let mut on = off.clone();
    on.enabled = true;
    on.allowed_domains.clear();
    assert!(
        r.control.save_settings(on.clone()).error.is_some(),
        "no domains"
    );

    on.allowed_domains = vec!["example.com".into()];
    *r.policy.0.lock().unwrap() = Err(PolicyError::Refused);
    assert!(
        r.control.save_settings(on.clone()).error.is_some(),
        "policy refuses"
    );
    *r.policy.0.lock().unwrap() = Ok(());

    *r.mail.profile.lock().unwrap() = Some(Ok("owner@example.org".into()));
    let refused = r.control.save_settings(on.clone());
    assert!(refused.error.is_some(), "sender outside the domains");
    assert!(
        !refused.settings.enabled,
        "a refusal returns what is stored"
    );

    *r.mail.profile.lock().unwrap() = None;
    on.verified_sender = "attacker@example.com".into();
    let saved = r.control.save_settings(on);
    assert_eq!(saved.error, None);
    assert!(saved.settings.enabled);
    assert_eq!(
        saved.settings.verified_sender, "owner@example.com",
        "server-set"
    );

    let mut slow = saved.settings.clone();
    slow.send_delay_minutes = 121;
    assert!(r.control.save_settings(slow).error.is_some());
}

// ============================================================ worker rules

#[tokio::test]
async fn a_row_left_sending_at_restart_needs_review() {
    let r = rig();
    let id = r.scheduled().await;
    store::cas(
        r.lock().conn(),
        &id,
        state::SCHEDULED,
        state::SENDING,
        store::Reasons::Keep,
        &[],
        1,
    )
    .unwrap();
    assert_eq!(r.worker.recover(), 1);
    assert_eq!(
        r.state(&id),
        (
            state::NEEDS_REVIEW.into(),
            vec!["send_outcome_unknown".into()]
        )
    );
    r.tick();
    assert!(r.mail.sent().is_empty(), "never an automatic second send");
}

#[test]
fn cancel_loses_to_a_send_already_running_and_wins_before_it() {
    // Cancel lands first: the send's compare-and-set loses; nothing is sent.
    let r = rig();
    let id = r.scheduled_sync();
    r.control.act(&id, OutboxAction::Cancel).unwrap();
    r.tick();
    assert!(r.mail.sent().is_empty());
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);

    // The send claimed the row first: Cancel is refused and changes nothing.
    let r = Arc::new(rig());
    let id = r.scheduled_sync();
    let (rr, rid) = (Arc::clone(&r), id.clone());
    let outcome: Arc<Mutex<Option<Result<(), String>>>> = Arc::default();
    let seen = Arc::clone(&outcome);
    *r.mail.during_send.lock().unwrap() = Some(Box::new(move || {
        let result = rr.control.act(&rid, OutboxAction::Cancel).map(|_| ());
        *seen.lock().unwrap() = Some(result);
    }));
    // `act` needs the library lock, which `send_one` does not hold while
    // `gws` runs.
    r.tick();
    assert!(matches!(*outcome.lock().unwrap(), Some(Err(_))));
    assert_eq!(r.row(&id).state, state::SENT);
    assert_eq!(r.mail.sent().len(), 1);
    *r.mail.during_send.lock().unwrap() = None;
}

impl Rig {
    /// [`Rig::scheduled`] without an async context.
    fn scheduled_sync(&self) -> String {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(self.scheduled())
    }
}

#[tokio::test]
async fn a_due_row_sends_within_30_seconds_of_send_at() {
    let r = rig();
    r.auto_ready();
    let id = r.meeting();
    r.pass().await;
    let send_at = r.row(&id).send_at.unwrap();
    // The send loop ticks every 15 s from an arbitrary phase.
    let mut t = r.now() + 7_000;
    while r.row(&id).state == state::SCHEDULED {
        r.now.store(t, Ordering::SeqCst);
        r.tick();
        t += super::worker::SEND_INTERVAL.as_millis() as i64;
        assert!(t < send_at + 120_000, "never sent");
    }
    assert_eq!(r.row(&id).state, state::SENT);
    let sent_at = r.row(&id).sent_at.unwrap();
    assert!(
        sent_at >= send_at && sent_at - send_at <= 30_000,
        "{}",
        sent_at - send_at
    );
}

#[tokio::test]
async fn a_row_overdue_while_paused_goes_to_review_instead_of_sending() {
    let r = rig();
    let id = r.scheduled().await;
    *r.mail.profile.lock().unwrap() = Some(Err(MailError::SpawnFailed));
    r.tick();
    assert_eq!(
        r.control.outbox().paused.as_deref(),
        Some("gws_unavailable")
    );
    assert_eq!(r.row(&id).state, state::SCHEDULED);
    r.advance(DELAYED_MS + MIN);
    *r.mail.profile.lock().unwrap() = None;
    r.tick();
    assert_eq!(
        r.state(&id),
        (
            state::NEEDS_REVIEW.into(),
            vec!["delayed_while_paused".into()]
        )
    );
    assert!(r.mail.sent().is_empty());
}

#[tokio::test]
async fn a_first_time_recipient_forces_review_in_auto_mode() {
    let r = rig();
    r.seed_decisions(ENGINE, 20, 0);
    r.know(&["ana@example.com"]); // bo has never been sent a release
    r.store(&enabled(ReleaseMode::Auto));
    let id = r.meeting();
    r.pass().await;
    assert_eq!(
        r.state(&id),
        (
            state::NEEDS_REVIEW.into(),
            vec!["first_time_recipient".into()]
        )
    );
    // Once a release reaches bo, the next one may go automatically.
    r.control.act(&id, OutboxAction::Approve).unwrap();
    r.tick();
    let next = r.meeting_at(T0 + 5 * HOUR, 10, five_attendees());
    r.pass().await;
    assert_eq!(r.row(&next).state, state::SCHEDULED);
}

#[tokio::test]
async fn auto_unlocks_from_human_decisions_for_the_current_engine() {
    let r = rig();
    let auto = enabled(ReleaseMode::Auto);
    r.seed_decisions(ENGINE, 19, 0);
    assert!(!r.control.settings().auto_unlocked);
    assert!(
        r.control.save_settings(auto.clone()).error.is_some(),
        "19 is not enough"
    );
    r.seed_decisions(ENGINE, 0, 5); // 19 of 24 = 79%
    assert!(!r.control.settings().auto_unlocked);
    r.seed_decisions(ENGINE, 1, 0); // 20 of 25 = 80%
    assert!(r.control.settings().auto_unlocked);
    // `auto` and `sent_again` decisions do not count.
    let db_engine = "other-engine";
    {
        let db = r.lock();
        for _ in 0..30 {
            store::record_decision(db.conn(), "m", db_engine, decision::AUTO, 1).unwrap();
            store::record_decision(db.conn(), "m", db_engine, decision::SENT_AGAIN, 1).unwrap();
        }
    }
    assert_eq!(r.control.save_settings(auto).error, None);

    // An auto-scheduled row; then the engine changes: auto locks again.
    r.know(&["ana@example.com", "bo@example.com"]);
    let id = r.meeting();
    r.pass().await;
    assert_eq!(r.row(&id).state, state::SCHEDULED);
    *r.preparer.engine.lock().unwrap() = db_engine.to_owned();
    r.tick();
    assert_eq!(
        r.state(&id),
        (state::NEEDS_REVIEW.into(), vec!["mode_changed".into()])
    );
    assert!(r.mail.sent().is_empty());
    let next = r.meeting_at(T0 + 5 * HOUR, 10, five_attendees());
    r.pass().await;
    assert_eq!(
        r.state(&next),
        (state::NEEDS_REVIEW.into(), vec!["auto_locked".into()])
    );
    assert!(!r.control.settings().auto_unlocked);
}

#[tokio::test]
async fn the_fingerprint_ignores_notes_and_the_title() {
    let r = rig();
    let id = r.reviewed().await;
    let before = r.row(&id).input_fingerprint.unwrap();
    {
        let mut db = r.lock();
        db.meetings().set_title(&id, "A renamed meeting").unwrap();
        db.meetings()
            .upsert_note(&id, "private note text", &[])
            .unwrap();
    }
    r.pass().await;
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);
    let row = r.row(&id);
    let (_, now) = load_input(&mut r.lock(), &row).unwrap();
    assert_eq!(now, before);

    // Text, speaker, offset and corrections each change it.
    let base = load_input(&mut r.lock(), &row).unwrap().0;
    let print =
        |s: &[super::types::SourceSegment], c: &[super::types::NameCorrection]| fingerprint(s, c);
    let original = print(&base.segments, &base.corrections);
    let mut moved = base.segments.clone();
    moved[0].offset_ms += 1;
    assert_ne!(print(&moved, &base.corrections), original);
    let mut spoken = base.segments.clone();
    spoken[0].speaker = "S9".into();
    assert_ne!(print(&spoken, &base.corrections), original);
    let corrected = vec![super::types::NameCorrection {
        from: "Ana".into(),
        to: "Anna".into(),
    }];
    assert_ne!(print(&base.segments, &corrected), original);
}

#[tokio::test]
async fn typed_policy_errors_map_to_pause_retry_and_restart() {
    // Refused → paused `engine_unavailable`; nothing created or prepared.
    let r = rig();
    *r.policy.0.lock().unwrap() = Err(PolicyError::Refused);
    let id = r.meeting();
    r.pass().await;
    assert!(store::get(r.lock().conn(), &id).unwrap().is_none());
    assert_eq!(
        r.control.outbox().paused.as_deref(),
        Some("engine_unavailable")
    );
    *r.policy.0.lock().unwrap() = Ok(());
    r.pass().await;
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);
    assert_eq!(r.control.outbox().paused, None);

    // A preparer that reports the refusal pauses too, and the row waits.
    let r = rig();
    r.preparer.failing("review", PrepareError::PolicyRefused);
    let id = r.meeting();
    r.pass().await;
    assert_eq!(r.row(&id).state, state::PREPARING);
    assert_eq!(
        r.control.outbox().paused.as_deref(),
        Some("engine_unavailable")
    );

    // SettingsChanged → stays preparing, retried next pass.
    let r = rig();
    r.preparer
        .failing("summarize", PrepareError::SettingsChanged);
    let id = r.meeting();
    r.pass().await;
    assert_eq!(r.row(&id).state, state::PREPARING);
    r.preparer.heal();
    r.pass().await;
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);

    // InputsChanged (or inputs moving mid-call) → preparation restarts.
    let r = rig();
    let id = r.meeting();
    let db = Arc::clone(&r.db);
    let target = id.clone();
    let mut once = true;
    *r.preparer.during_summary.lock().unwrap() = Some(Box::new(move || {
        if std::mem::take(&mut once) {
            db.lock()
                .unwrap()
                .conn()
                .execute(
                    "UPDATE segments SET text = 'late words' WHERE meeting_id = ?1 AND idx = 2",
                    [&target],
                )
                .unwrap();
        }
    }));
    r.pass().await;
    assert_eq!(
        r.row(&id).state,
        state::PREPARING,
        "the stale result is not stored"
    );
    r.pass().await;
    let row = r.row(&id);
    assert_eq!(row.state, state::NEEDS_REVIEW);
    assert!(row.attachment.unwrap().contains("late words"));
}

#[tokio::test]
async fn eligibility_reasons_are_cached_and_reevaluated_after_15_minutes() {
    let r = rig();
    // No confident match: the event is gone from the calendar.
    let id = r.meeting();
    r.events.lock().unwrap().clear();
    r.pass().await;
    assert_eq!(r.control.meeting_status(&id).reasons, ["event_not_found"]);
    let cached = store::eligibility(r.lock().conn(), &id).unwrap().unwrap();
    assert_eq!(cached.1, r.now());
    // It comes back within 15 minutes: still cached, not re-evaluated.
    r.events
        .lock()
        .unwrap()
        .push(event(&format!("evt-{id}"), T0, five_attendees()));
    r.advance(10 * MIN);
    r.pass().await;
    assert!(store::get(r.lock().conn(), &id).unwrap().is_none());
    r.advance(6 * MIN);
    r.pass().await;
    assert_eq!(r.row(&id).state, state::NEEDS_REVIEW);
    assert!(store::eligibility(r.lock().conn(), &id).unwrap().is_none());

    // Not ready.
    let r = rig();
    let id = r.meeting();
    r.lock().meetings().set_state(&id, "transcribing").unwrap();
    r.pass().await;
    assert_eq!(r.control.meeting_status(&id).reasons, ["not_ready"]);

    // No calendar link at all.
    let r = rig();
    let id = r.meeting();
    r.lock()
        .conn()
        .execute(
            "UPDATE meetings SET calendar_uid = NULL WHERE id = ?1",
            [&id],
        )
        .unwrap();
    r.pass().await;
    assert_eq!(
        r.control.meeting_status(&id).reasons,
        ["no_confident_match"]
    );

    // Too long.
    let r = rig();
    let id = r.meeting_at(T0, 4_001, five_attendees());
    r.pass().await;
    assert_eq!(r.control.meeting_status(&id).reasons, ["too_long"]);

    // Outside the 7-day window: no row, ever.
    let r = rig();
    let id = r.meeting();
    r.advance(8 * 24 * 60 * MIN);
    r.pass().await;
    assert!(store::get(r.lock().conn(), &id).unwrap().is_none());
    assert_eq!(r.control.meeting_status(&id).reasons, ["window_passed"]);
}

#[tokio::test]
async fn at_most_five_rows_are_created_and_prepared_per_pass() {
    let r = rig();
    let ids: Vec<String> = (0..7)
        .map(|i| r.meeting_at(T0 + i * HOUR, 10, five_attendees()))
        .collect();
    r.pass().await;
    let rows = store::in_states(r.lock().conn(), &[state::NEEDS_REVIEW]).unwrap();
    assert_eq!(rows.len(), 5);
    assert!(
        rows.iter().all(|row| ids[..5].contains(&row.meeting_id)),
        "oldest first"
    );
    r.pass().await;
    assert_eq!(
        store::in_states(r.lock().conn(), &[state::NEEDS_REVIEW])
            .unwrap()
            .len(),
        7
    );
}

#[tokio::test]
async fn the_owner_notice_goes_out_when_a_row_needs_review() {
    let r = rig();
    let mut s = enabled(ReleaseMode::Approval);
    s.notify_address = "owner@example.com".into();
    r.store(&s);
    r.reviewed().await;
    r.tick();
    let sent = r.mail.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, ["owner@example.com"]);
    assert_eq!(
        sent[0].subject,
        "FlyOnTheWall: 1 meeting waiting for release review"
    );
    r.advance(MIN);
    r.tick();
    assert_eq!(r.mail.sent().len(), 1, "at most hourly");
}

#[tokio::test]
async fn the_outbox_lists_waiting_rows_oldest_first_with_the_badge() {
    let r = rig();
    let first = r.reviewed().await;
    let second = r.meeting_at(T0 + HOUR, 10, five_attendees());
    r.preparer.failing("cut", PrepareError::ModelFailed);
    r.advance(1);
    r.pass().await;
    let outbox = r.control.outbox();
    let ids: Vec<&str> = outbox.items.iter().map(|i| i.meeting_id.as_str()).collect();
    assert_eq!(ids, [first.as_str(), second.as_str()]);
    assert_eq!(outbox.badge, 2);
    let detail = r.control.detail(&first).unwrap();
    assert_eq!(detail.recipients.len(), 2);
    assert_eq!(detail.segments.len(), 10);
    assert!(detail.segments.iter().all(|s| s.retained));
    assert_eq!(detail.subject, "Notes: Planning sync (2026-09-21)");
    assert!(r.control.detail("no-such-meeting").is_none());
}

// ================================================== wrong calendar event

impl Rig {
    /// A meeting left in `which` (`preparing`, `needs_review`, `scheduled`
    /// or `held`).
    async fn in_state(&self, which: &str) -> String {
        match which {
            state::PREPARING => {
                self.preparer.failing("cut", PrepareError::SettingsChanged);
                let id = self.meeting();
                self.pass().await;
                self.preparer.heal();
                id
            }
            state::NEEDS_REVIEW => self.reviewed().await,
            state::SCHEDULED => self.scheduled().await,
            state::HELD => {
                self.preparer.failing("review", PrepareError::ModelFailed);
                let id = self.meeting();
                self.pass().await;
                self.preparer.heal();
                id
            }
            other => panic!("no rig for {other}"),
        }
    }
}

#[tokio::test]
async fn a_new_row_stores_the_matched_events_scheduled_times() {
    let r = rig();
    r.preparer.failing("cut", PrepareError::SettingsChanged);
    let id = r.meeting();
    r.pass().await;
    let row = r.row(&id);
    assert_eq!(row.state, state::PREPARING);
    assert_eq!(row.event_start_ms, Some(T0));
    assert_eq!(row.event_end_ms, Some(T0 + 30 * MIN));

    // The meeting line and the Outbox carry the match.
    let status = r.control.meeting_status(&id);
    assert_eq!(status.event_title.as_deref(), Some("Planning sync"));
    assert_eq!(status.event_start_ms, Some(T0));
    assert_eq!(status.event_end_ms, Some(T0 + 30 * MIN));

    r.preparer.heal();
    r.pass().await;
    let item = &r.control.outbox().items[0];
    assert_eq!(item.event_title, "Planning sync");
    assert_eq!(item.event_start_ms, Some(T0));
    assert_eq!(item.event_end_ms, Some(T0 + 30 * MIN));
    let detail = r.control.detail(&id).unwrap();
    assert_eq!(detail.item.event_start_ms, Some(T0));
}

#[tokio::test]
async fn a_row_from_before_event_times_shows_the_title_only() {
    let r = rig();
    let id = r.reviewed().await;
    r.lock()
        .conn()
        .execute(
            "UPDATE meeting_releases SET event_start_ms = NULL, event_end_ms = NULL",
            [],
        )
        .unwrap();
    let status = r.control.meeting_status(&id);
    assert_eq!(status.event_title.as_deref(), Some("Planning sync"));
    assert_eq!((status.event_start_ms, status.event_end_ms), (None, None));
    let item = &r.control.outbox().items[0];
    assert_eq!((item.event_start_ms, item.event_end_ms), (None, None));
}

#[tokio::test]
async fn a_meeting_without_a_row_names_no_event() {
    let r = rig();
    let status = r.control.meeting_status("no-such-meeting");
    assert_eq!(status.state, "not_eligible");
    assert_eq!(status.event_title, None);
    assert_eq!((status.event_start_ms, status.event_end_ms), (None, None));
}

#[tokio::test]
async fn wrong_event_drops_from_every_open_state() {
    for which in [
        state::PREPARING,
        state::NEEDS_REVIEW,
        state::SCHEDULED,
        state::HELD,
    ] {
        let r = rig();
        let id = r.in_state(which).await;
        assert_eq!(r.row(&id).state, which);
        let decided = r.decisions();
        let detail = r.control.act(&id, OutboxAction::WrongEvent).unwrap();
        assert_eq!(detail.item.state, state::DROPPED, "{which}");
        let row = r.row(&id);
        assert_eq!(
            (row.state.as_str(), row.reasons.clone()),
            (state::DROPPED, vec!["wrong_event".to_owned()]),
            "{which}"
        );
        assert_eq!(row.send_at, None, "{which}");
        let mut expected = decided.clone();
        expected.push(decision::WRONG_EVENT.to_owned());
        assert_eq!(r.decisions(), expected, "{which}");
        // Nothing goes out afterwards, and the worker does not pick it up.
        r.advance(30 * MIN);
        r.tick();
        r.pass().await;
        assert!(r.mail.sent().is_empty(), "{which}");
        assert_eq!(r.row(&id).state, state::DROPPED, "{which}");
        let status = r.control.meeting_status(&id);
        assert_eq!(status.state, "dropped");
        assert_eq!(status.reasons, ["wrong_event"]);
    }
}

#[tokio::test]
async fn wrong_event_is_refused_once_sent_or_dropped() {
    let r = rig();
    let sent = r.scheduled().await;
    r.tick();
    assert_eq!(r.row(&sent).state, state::SENT);
    assert!(r.control.act(&sent, OutboxAction::WrongEvent).is_err());
    assert_eq!(r.row(&sent).state, state::SENT);

    let dropped = r.reviewed_second().await;
    r.control.act(&dropped, OutboxAction::Drop).unwrap();
    let before = r.decisions();
    assert!(r.control.act(&dropped, OutboxAction::WrongEvent).is_err());
    assert_eq!(
        r.state(&dropped),
        (state::DROPPED.into(), vec!["by_user".into()])
    );
    assert_eq!(r.decisions(), before, "a refusal records nothing");
}

/// The row's state while `gws` ran, and what Wrong meeting got then.
type Seen = (String, Result<(), String>);

#[test]
fn wrong_event_is_refused_while_sending() {
    let r = Arc::new(rig());
    let id = r.scheduled_sync();
    let (rr, rid) = (Arc::clone(&r), id.clone());
    let outcome: Arc<Mutex<Option<Seen>>> = Arc::default();
    let seen = Arc::clone(&outcome);
    *r.mail.during_send.lock().unwrap() = Some(Box::new(move || {
        let during = rr.row(&rid).state;
        let result = rr.control.act(&rid, OutboxAction::WrongEvent).map(|_| ());
        *seen.lock().unwrap() = Some((during, result));
    }));
    r.tick();
    let (during, result) = outcome.lock().unwrap().take().unwrap();
    assert_eq!(during, state::SENDING);
    assert!(result.is_err());
    assert_eq!(r.row(&id).state, state::SENT);
    assert_eq!(r.decisions(), [decision::APPROVED_UNCHANGED]);
    *r.mail.during_send.lock().unwrap() = None;
}

#[tokio::test]
async fn wrong_event_decisions_do_not_count_toward_the_auto_unlock() {
    let r = rig();
    let id = r.reviewed().await;
    r.control.act(&id, OutboxAction::WrongEvent).unwrap();
    assert_eq!(
        store::decision_counts(r.lock().conn(), ENGINE).unwrap(),
        (0, 0),
        "neither numerator nor denominator"
    );
    // 20 of 20 unlocks; ten wrong-event rejections must not dilute it.
    r.seed_decisions(ENGINE, 20, 0);
    {
        let db = r.lock();
        for _ in 0..10 {
            store::record_decision(db.conn(), "gone", ENGINE, decision::WRONG_EVENT, 1).unwrap();
        }
    }
    assert_eq!(
        store::decision_counts(r.lock().conn(), ENGINE).unwrap(),
        (20, 20)
    );
    assert!(r.control.settings().auto_unlocked);
}

#[tokio::test]
async fn a_wrong_event_meeting_is_never_prepared_again() {
    let r = rig();
    let id = r.reviewed().await;
    let calls = r.preparer.calls().len();
    r.control.act(&id, OutboxAction::WrongEvent).unwrap();
    // Past the eligibility cache, with the meeting edited and the calendar
    // still offering the same event.
    r.lock()
        .conn()
        .execute(
            "UPDATE meetings SET updated_at = updated_at + 1 WHERE id = ?1",
            [&id],
        )
        .unwrap();
    for _ in 0..3 {
        r.advance(20 * MIN);
        r.pass().await;
        r.tick();
    }
    assert_eq!(
        r.state(&id),
        (state::DROPPED.into(), vec!["wrong_event".into()])
    );
    assert_eq!(r.preparer.calls().len(), calls, "never prepared again");
    assert!(r.mail.sent().is_empty());
    let rows: i64 = r
        .lock()
        .conn()
        .query_row(
            "SELECT count(*) FROM meeting_releases WHERE meeting_id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
}
