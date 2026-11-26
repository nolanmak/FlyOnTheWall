use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};

use crate::enrich::{
    EnrichReceipt, EnrichReport, RECEIPTS_KEY, read_receipts, stamp, start_receipt,
};

type ReadHook = Box<dyn FnOnce()>;
type BusySignal = (Sender<()>, Receiver<()>);

thread_local! {
    static AFTER_READ: RefCell<Option<ReadHook>> = const { RefCell::new(None) };
    static ON_BUSY: RefCell<Option<BusySignal>> = const { RefCell::new(None) };
}

pub(crate) fn after_read() {
    if let Some(hook) = AFTER_READ.take() {
        hook();
    }
}

pub(crate) struct Fixture(tempfile::TempDir);

impl Fixture {
    pub(crate) fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    pub(crate) fn open(&self) -> Db {
        Db::open(
            self.0.path().join("fixture.db"),
            &DbKey::from_bytes([1; 32]),
        )
        .unwrap()
    }
}

const LIMIT: Duration = Duration::from_secs(10);

pub(crate) fn independent_writers(key: &str, write: fn(&mut Db, &str, u64)) {
    let fixture = Fixture::new();
    let (mut first, second) = (fixture.open(), fixture.open());
    let first_id = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let second_id = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let id = first_id.clone();
    let other = second_id.clone();
    interleave(
        first,
        second,
        move |db| write(db, &id, 10),
        move |db| write(db, &other, 20),
    );
    let raw = fixture.open().get_setting(key).unwrap().unwrap();
    let entries: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        entries.get(&first_id).is_some(),
        "missing meeting A: {entries}"
    );
    assert!(
        entries.get(&second_id).is_some(),
        "missing meeting B: {entries}"
    );
}

fn interleave(
    mut first: Db,
    mut second: Db,
    write_first: impl FnOnce(&mut Db) + Send + 'static,
    write_second: impl FnOnce(&mut Db) + Send + 'static,
) {
    let (read_tx, read_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    let first_writer = std::thread::spawn(move || {
        AFTER_READ.set(Some(Box::new(move || {
            read_tx.send(()).unwrap();
            release_rx.recv_timeout(LIMIT).unwrap();
        })));
        write_first(&mut first);
        finished_tx.send(()).unwrap();
    });
    read_rx.recv_timeout(LIMIT).unwrap();

    // With the old code B completes while A still has its stale read.
    // With BEGIN IMMEDIATE B signals contention before it can read the map.
    let second_writer = std::thread::spawn(move || {
        ON_BUSY.set(Some((release_tx, finished_rx)));
        second
            .conn()
            .busy_handler(Some(|attempts| {
                if let Some((release, finished)) = ON_BUSY.take() {
                    release.send(()).unwrap();
                    finished.recv_timeout(LIMIT).unwrap();
                }
                attempts == 0
            }))
            .unwrap();
        write_second(&mut second);
        if let Some((release, finished)) = ON_BUSY.take() {
            release.send(()).unwrap();
            finished.recv_timeout(LIMIT).unwrap();
        }
    });
    first_writer.join().unwrap();
    second_writer.join().unwrap();
}

fn enrich(db: &mut Db, id: &str, clock: u64) {
    let mut report = EnrichReport::default();
    let title = format!("Fixture {clock}");
    stamp(
        db,
        id,
        &EnrichReceipt {
            started_at_ms: clock,
            finished_at_ms: clock + 1,
            minted_title: title.clone(),
        },
        Some(&title),
        &mut report,
    );
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

#[test]
fn receipt_enrichment_independent_writers() {
    independent_writers(RECEIPTS_KEY, enrich);
}

#[test]
fn receipt_legacy_adoption_preserves_concurrent_stamp() {
    let fixture = Fixture::new();
    let (mut first, second) = (fixture.open(), fixture.open());
    let mut legacy = NewMeeting::new("fixture", "UTC");
    legacy.title = "Check the synthetic receipt today".into();
    let legacy_id = first.meetings().create(legacy).unwrap();
    let transcript = first
        .meetings()
        .create_transcript(&legacy_id, "deepgram", "nova-3", true)
        .unwrap();
    first
        .meetings()
        .append_segments(
            &transcript,
            &[NewSegment::new(0, 0, 4000, "Check the synthetic receipt today").channel("system")],
        )
        .unwrap();
    let id = legacy_id.clone();
    let other = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let concurrent_id = other.clone();
    interleave(
        first,
        second,
        move |db| {
            let result = crate::enrich::adopt_legacy_titles(db);
            assert!(result.problems.is_empty(), "{:?}", result.problems);
            assert_eq!(result.adopted, [id]);
        },
        move |db| enrich(db, &concurrent_id, 20),
    );
    let entries = read_receipts(&fixture.open());
    assert_eq!(
        entries[&legacy_id].minted_title,
        "Check the synthetic receipt today"
    );
    assert_eq!(
        entries.get(&other).map(|receipt| receipt.started_at_ms),
        Some(20)
    );
}

fn start(db: &mut Db, id: &str, clock: u64) -> EnrichReceipt {
    let mut report = EnrichReport::default();
    let receipt = start_receipt(db, id, clock, &mut report);
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    receipt
}

fn mint_and_finish(
    db: &mut Db,
    id: &str,
    mut receipt: EnrichReceipt,
    title: &str,
    finished_at_ms: u64,
) {
    let inputs = db
        .setting_version(crate::engine::SETTINGS_KEY)
        .unwrap()
        .with_meeting_inputs(db, id)
        .unwrap();
    db.meetings()
        .set_title_if_setting(id, title, &inputs)
        .unwrap();
    receipt.finished_at_ms = finished_at_ms;
    let mut report = EnrichReport::default();
    stamp(db, id, &receipt, Some(title), &mut report);
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

fn assert_minted_title(db: &mut Db, id: &str, expected: &str) {
    let title = db.meetings().get(id).unwrap().title;
    assert_eq!(title, expected);
    assert_eq!(read_receipts(db)[id].minted_title, title);
}

#[test]
fn receipt_enrichment_start_preserves_concurrent_stamp() {
    let fixture = Fixture::new();
    let (mut first, second) = (fixture.open(), fixture.open());
    let a = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let b = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let (starting, finishing) = (a.clone(), b.clone());
    interleave(
        first,
        second,
        move |db| {
            start(db, &starting, 10);
        },
        move |db| enrich(db, &finishing, 20),
    );
    let entries = read_receipts(&fixture.open());
    assert_eq!(
        (entries[&a].started_at_ms, entries[&a].finished_at_ms),
        (10, 0)
    );
    assert_eq!(entries[&b].finished_at_ms, 21);
}

#[cfg(unix)]
#[test]
fn receipt_paid_retitle_preserves_concurrent_stamp() {
    use fotw_secrets::InMemoryKeyStore;

    let fixture = Fixture::new();
    let (mut first, second) = (fixture.open(), fixture.open());
    let a = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let b = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let transcript = first
        .meetings()
        .create_transcript(&a, "deepgram", "nova-3", true)
        .unwrap();
    first
        .meetings()
        .append_segments(
            &transcript,
            &[NewSegment::new(0, 0, 1000, "Discuss the synthetic receipt")],
        )
        .unwrap();
    enrich(&mut first, &a, 10);

    let binary = fixture.0.path().join(crate::testing::STUB_ENGINE_NAME);
    std::fs::write(&binary, concat!(
        "#!/bin/sh\ncat >/dev/null\n",
        "printf '%s\\n' '{\"type\":\"result\",\"is_error\":false,\"result\":\"Fixture Retitle\"}'\n",
    )).unwrap();
    crate::testing::make_executable(&binary);
    let settings = crate::engine::SummarizeSettings {
        cli_enabled: true,
        acknowledged_egress: true,
        binary: binary.to_string_lossy().into_owned(),
        ..Default::default()
    };
    first
        .put_setting(
            crate::engine::SETTINGS_KEY,
            &serde_json::to_string(&settings).unwrap(),
        )
        .unwrap();

    let (retitled, finishing) = (a.clone(), b.clone());
    interleave(
        first,
        second,
        move |db| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let problems = runtime.block_on(crate::enrich::retitle_meetings(
                db,
                &InMemoryKeyStore::new(),
                &[retitled],
            ));
            assert!(problems.is_empty(), "{problems:?}");
        },
        move |db| enrich(db, &finishing, 20),
    );

    let mut reader = fixture.open();
    assert_minted_title(&mut reader, &a, "Fixture Retitle");
    let entries = read_receipts(&reader);
    assert_eq!(
        (entries[&a].started_at_ms, entries[&a].finished_at_ms),
        (10, 11)
    );
    assert_eq!(entries[&b].finished_at_ms, 21);
}

#[test]
fn receipt_overlapping_passes_record_the_title_written_last() {
    let fixture = Fixture::new();
    let (mut first, mut second) = (fixture.open(), fixture.open());
    let id = first
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let a = start(&mut first, &id, 10);
    let b = start(&mut second, &id, 20);
    mint_and_finish(&mut second, &id, b, "Second pass title", 21);
    mint_and_finish(&mut first, &id, a, "First pass title", 30);
    assert_minted_title(&mut fixture.open(), &id, "First pass title");
}

#[test]
fn receipt_pass_after_clock_step_back_records_minted_title() {
    let fixture = Fixture::new();
    let mut db = fixture.open();
    let id = db
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let old = start(&mut db, &id, 5_000);
    mint_and_finish(&mut db, &id, old, "Original pass title", 5_001);
    let new = start(&mut db, &id, 4_000);
    assert_eq!(new.minted_title, "Original pass title");
    let on_disk_start = read_receipts(&fixture.open())[&id].clone();
    mint_and_finish(&mut db, &id, new, "Replacement pass title", 4_002);
    assert_minted_title(&mut fixture.open(), &id, "Replacement pass title");
    assert_eq!(
        (on_disk_start.started_at_ms, on_disk_start.finished_at_ms),
        (4_000, 0)
    );
    let finish = &read_receipts(&fixture.open())[&id];
    assert_eq!(
        (finish.started_at_ms, finish.finished_at_ms),
        (4_000, 4_002)
    );
}

#[test]
fn receipt_interrupted_writer_keeps_the_blob() {
    let fixture = Fixture::new();
    let (mut first, mut second) = (fixture.open(), fixture.open());
    enrich(&mut first, "fixture", 10);
    let before = second.get_setting(RECEIPTS_KEY).unwrap();
    AFTER_READ.set(Some(Box::new(|| panic!("interrupt receipt write"))));
    assert!(catch_unwind(AssertUnwindSafe(|| enrich(&mut first, "fixture", 20))).is_err());
    assert_eq!(second.get_setting(RECEIPTS_KEY).unwrap(), before);
    enrich(&mut second, "fixture", 30);
}

#[test]
fn receipt_enrichment_legacy_blob() {
    let fixture = Fixture::new();
    let mut db = fixture.open();
    let legacy = r#"{ "fixture": {"minted_title":"Fixture title"} }"#;
    db.put_setting(RECEIPTS_KEY, legacy).unwrap();
    drop(db);
    let reader = fixture.open();
    let entries = read_receipts(&reader);
    assert_eq!(entries["fixture"].minted_title, "Fixture title");
    assert_eq!(entries["fixture"].started_at_ms, 0);
    assert_eq!(entries["fixture"].finished_at_ms, 0);
    assert_eq!(
        reader.get_setting(RECEIPTS_KEY).unwrap().as_deref(),
        Some(legacy)
    );
}

#[test]
fn receipt_legacy_blob_update_preserves_other_entries() {
    let fixture = Fixture::new();
    let mut db = fixture.open();
    let legacy = r#"{
        "updated": {"minted_title":"Original title"},
        "older": {"minted_title":"Fixture café"},
        "finished": {"started_at_ms":5,"finished_at_ms":8,"minted_title":"Fixture note","future_field":true}
    }"#;
    db.put_setting(RECEIPTS_KEY, legacy).unwrap();
    enrich(&mut db, "updated", 10);

    let raw = fixture.open().get_setting(RECEIPTS_KEY).unwrap().unwrap();
    let entries: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // Base reserializes typed entries, expands missing defaults, and drops unknown fields.
    assert_eq!(
        entries,
        serde_json::json!({
            "updated": {"started_at_ms":10,"finished_at_ms":11,"minted_title":"Fixture 10"},
            "older": {"started_at_ms":0,"finished_at_ms":0,"minted_title":"Fixture café"},
            "finished": {"started_at_ms":5,"finished_at_ms":8,"minted_title":"Fixture note"}
        })
    );
}

#[test]
fn receipt_writer_waits_with_default_busy_timeout() {
    let fixture = Fixture::new();
    let (first, mut second) = (fixture.open(), fixture.open());
    let timeout: i64 = second
        .conn()
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(timeout, 5000);
    first.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
    let (start_tx, start_rx) = channel();
    let writer = std::thread::spawn(move || {
        let began = Instant::now();
        start_tx.send(()).unwrap();
        enrich(&mut second, "fixture", 10);
        began.elapsed()
    });
    start_rx.recv_timeout(LIMIT).unwrap();
    let hold = Duration::from_millis(100);
    std::thread::sleep(hold);
    first.conn().execute_batch("COMMIT").unwrap();
    assert!(writer.join().unwrap() >= hold);
    assert_eq!(read_receipts(&fixture.open())["fixture"].started_at_ms, 10);
}
