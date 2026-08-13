//! The wired retention engine: promotion, the sweeper, and the scheduler.
//!
//! `fotw-pipeline`'s tests prove the *decision* is right. These prove it is
//! actually connected — that a finished session reaches `media/`, that the
//! sweeper reads the real library rather than a fixture, and that the thing
//! runs at all. A retention engine that is correct and never invoked is
//! indistinguishable from one that does not exist, which is precisely the
//! state this issue found the code in.
//!
//! Everything here uses a fake clock and a temp directory. A test that waits
//! thirty days is not a test.

use std::path::{Path, PathBuf};

use fotw_pipeline::wal::SessionWal;
use fotw_store::{Db, DbKey, NewMeeting, NewRecording, NewSegment};
use fotwd::retention::{self, LIVE_SESSION_WINDOW_MS, Schedule, SweepMode, Tick};

const RATE: u32 = 16_000;
const MS_PER_DAY: u64 = 86_400_000;
/// 2026-08-13T00:00:00Z.
const NOW: u64 = 1_786_579_200_000;
const MIB: u64 = 1024 * 1024;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "fotwd-retention-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn db_at(root: &Path) -> Db {
    // The real encrypted path with a fixed key: CI has no keychain, and an
    // in-memory database would not exercise the `<root>/db.sqlite3` layout the
    // sweeper resolves media paths against.
    Db::open(root.join("db.sqlite3"), &DbKey::from_bytes([0xcd; 32])).unwrap()
}

fn tone(hz: f32, secs: f32) -> Vec<f32> {
    let n = (RATE as f32 * secs) as usize;
    (0..n)
        .map(|i| 0.5 * (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin())
        .collect()
}

/// A finished session directory with real audio in it.
fn finished_session(root: &Path) -> PathBuf {
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut wal = SessionWal::create(&sessions, RATE, 1).unwrap();
    wal.write_system(&tone(440.0, 1.0)).unwrap();
    wal.write_mic(&tone(880.0, 1.0)).unwrap();
    wal.finalize().unwrap()
}

/// A meeting whose audio is already in the media tree, with `bytes` per leg.
///
/// `transcribed` is the switch the protection tests turn off: everything else
/// about the two meetings is identical.
fn seeded_meeting(
    db: &mut Db,
    root: &Path,
    label: &str,
    started_at_ms: u64,
    bytes: u64,
    transcribed: bool,
) -> String {
    let mut m = NewMeeting::new("dev-1", "UTC");
    m.title = label.to_owned();
    m.started_at_ms = started_at_ms as i64;
    let id = db.meetings().create(m).unwrap();

    if transcribed {
        let t = db
            .meetings()
            .create_transcript(&id, "deepgram", "nova-3", true)
            .unwrap();
        db.meetings()
            .append_segments(
                &t,
                &[NewSegment {
                    idx: 0,
                    start_ms: 0,
                    end_ms: 900,
                    channel: "system".into(),
                    speaker_label: None,
                    person_id: None,
                    text: format!("{label} said something worth keeping"),
                    confidence: Some(0.9),
                    is_final: true,
                    words: None,
                }],
            )
            .unwrap();
    }
    db.meetings().set_state(&id, "ready").unwrap();

    let rel_dir = format!("media/2026/08/{id}");
    std::fs::create_dir_all(root.join(&rel_dir)).unwrap();
    for channel in ["system", "mic"] {
        let rel = format!("{rel_dir}/{channel}.opus");
        std::fs::write(root.join(&rel), vec![7u8; bytes as usize / 2]).unwrap();
        db.upsert_recording(
            &id,
            &NewRecording {
                channel: channel.to_owned(),
                rel_path: rel,
                bytes: bytes / 2,
                duration_ms: 1_000,
                sample_rate_hz: RATE,
            },
        )
        .unwrap();
    }
    id
}

/// Rewind a meeting's transcript so the age rule can be exercised without
/// waiting. The retention clock starts at transcript-ready (§9.5 correction 2).
fn transcript_ready_at(db: &mut Db, meeting_id: &str, at_ms: u64) {
    db.conn()
        .execute(
            "UPDATE transcripts SET created_at = ?2 WHERE meeting_id = ?1",
            (meeting_id, at_ms as i64),
        )
        .unwrap();
}

// ------------------------------------------------------------------ promotion

#[test]
fn a_finished_session_is_promoted_into_the_media_tree_and_recorded_as_a_relative_path() {
    let root = tmpdir("promote");
    let mut db = db_at(&root);
    let dir = finished_session(&root);

    let mut m = NewMeeting::new("dev-1", "UTC");
    m.started_at_ms = NOW as i64;
    let meeting_id = db.meetings().create(m).unwrap();

    let promoted = retention::promote_session(&mut db, &root, &dir, &meeting_id, NOW).unwrap();

    assert!(!dir.exists(), "the session directory was never retired");
    assert_eq!(promoted.tracks.len(), 2);

    let rows = db.audio_inventory().unwrap();
    let row = rows.iter().find(|r| r.meeting_id == meeting_id).unwrap();
    assert_eq!(row.audio.len(), 2, "the promotion recorded no audio rows");
    for f in &row.audio {
        assert!(
            f.rel_path
                .starts_with(&format!("media/2026/08/{meeting_id}/")),
            "{} is not in the §9.2 layout",
            f.rel_path
        );
        assert!(!Path::new(&f.rel_path).is_absolute());
        let abs = root.join(&f.rel_path);
        assert!(
            abs.exists(),
            "{} was recorded but not written",
            abs.display()
        );
        assert_eq!(abs.metadata().unwrap().len(), f.bytes.unwrap());
        assert!(f.bytes.unwrap() > 0);
    }
}

#[test]
fn a_promotion_interrupted_before_the_database_row_is_finished_on_the_next_start() {
    // The crash window between "the media is on disk" and "the library knows
    // about it". Left unresolved, the audio is invisible to the sweeper, to
    // the player, and to `delete_meeting` — orphaned bytes that nothing will
    // ever reclaim.
    let root = tmpdir("resume");
    let mut db = db_at(&root);
    let dir = finished_session(&root);

    let mut m = NewMeeting::new("dev-1", "UTC");
    m.started_at_ms = NOW as i64;
    let meeting_id = db.meetings().create(m).unwrap();

    // Claim and publish, then "die" before the row is written.
    fotw_pipeline::promote::claim(&dir, &meeting_id, NOW).unwrap();
    fotw_pipeline::promote::encode(&dir).unwrap();
    let published = fotw_pipeline::promote::publish(&dir, &root).unwrap();
    fotw_pipeline::promote::stamp(&dir, &published).unwrap();
    assert!(db.audio_inventory().unwrap()[0].audio.is_empty());

    let done = retention::resume_promotions(&mut db, &root);
    assert_eq!(done.len(), 1, "the interrupted promotion was not found");
    assert!(done[0].is_ok(), "{done:?}");

    assert!(!dir.exists());
    let rows = db.audio_inventory().unwrap();
    assert_eq!(rows[0].audio.len(), 2, "the recording rows never landed");
}

#[test]
fn deleting_a_promoted_meeting_reaches_the_audio_promotion_actually_wrote() {
    // §9.6 unlinks `media/<yyyy>/<mm>/<meeting_id>/` recursively, and
    // `fotw-store` tests that against a hand-made fixture. This is the seam:
    // the directory promotion *really* produces has to be the one delete
    // *really* looks in. A layout drift between the two would leave every
    // deleted meeting's audio on disk, and every test on either side would
    // still pass.
    let root = tmpdir("delete");
    let mut db = db_at(&root);
    let dir = finished_session(&root);

    let mut m = NewMeeting::new("dev-1", "UTC");
    m.started_at_ms = NOW as i64;
    let meeting_id = db.meetings().create(m).unwrap();
    let promoted = retention::promote_session(&mut db, &root, &dir, &meeting_id, NOW).unwrap();

    let files: Vec<PathBuf> = promoted
        .tracks
        .iter()
        .map(|t| root.join(&t.rel_path))
        .collect();
    assert!(files.iter().all(|p| p.exists()));

    db.delete_meeting(&meeting_id).unwrap();

    for p in &files {
        assert!(
            !p.exists(),
            "deleting the meeting left its audio at {}",
            p.display()
        );
    }
    assert!(
        !root.join(&promoted.rel_dir).exists(),
        "the media directory survived the delete"
    );
}

// -------------------------------------------------------------------- sweeping

/// The rule §9.5 does not state and this project will not ship without.
///
/// `fotwd` marks a meeting `ready` when its session finishes whether or not a
/// provider was configured, so "state = ready" and "a transcript exists" are
/// different questions. Evicting on the first one deletes the only copy of
/// every meeting a user recorded with transcription switched off.
#[test]
fn the_sweeper_never_evicts_audio_for_a_meeting_with_no_transcript() {
    let root = tmpdir("no-transcript");
    let mut db = db_at(&root);

    // Both are `ready`, both are ancient, both are enormous. Only one has
    // words behind it.
    let with_text = seeded_meeting(
        &mut db,
        &root,
        "kept",
        NOW - 400 * MS_PER_DAY,
        8 * MIB,
        true,
    );
    let no_text = seeded_meeting(
        &mut db,
        &root,
        "orphan",
        NOW - 400 * MS_PER_DAY,
        8 * MIB,
        false,
    );
    transcript_ready_at(&mut db, &with_text, NOW - 399 * MS_PER_DAY);

    // Maximum pressure: a budget of zero, which is the setting under which a
    // sweeper with this bug deletes literally everything it is allowed to.
    retention::set_settings(
        &mut db,
        &fotw_pipeline::retention::RetentionSettings {
            default_days: 30,
            budget_bytes: 0,
        },
    )
    .unwrap();

    let report = retention::sweep(&mut db, &root, NOW, SweepMode::Apply).unwrap();

    let gone: Vec<&str> = report
        .plan
        .evictions
        .iter()
        .map(|e| e.meeting_id.as_str())
        .collect();
    assert_eq!(
        gone,
        [with_text.as_str()],
        "the sweeper touched a meeting that has no transcript"
    );

    // On disk, not just in the plan.
    let inventory = db.audio_inventory().unwrap();
    let orphan = inventory.iter().find(|r| r.meeting_id == no_text).unwrap();
    assert_eq!(
        orphan.audio.len(),
        2,
        "the untranscribed meeting's rows were retired"
    );
    for f in &orphan.audio {
        assert!(
            root.join(&f.rel_path).exists(),
            "the only copy of an untranscribed meeting was deleted: {}",
            f.rel_path
        );
    }
    // And it says so out loud rather than quietly giving up.
    assert!(
        report.render().contains("not been transcribed"),
        "the report is silent about the audio it could not reclaim:\n{}",
        report.render()
    );
}

#[test]
fn a_dry_run_deletes_nothing_and_names_every_file_it_would_delete() {
    // Deleting a user's meeting audio is irreversible, so the answer to "what
    // is about to happen" has to be available before it happens.
    let root = tmpdir("dry-run");
    let mut db = db_at(&root);
    let id = seeded_meeting(&mut db, &root, "old", NOW - 90 * MS_PER_DAY, 4 * MIB, true);
    transcript_ready_at(&mut db, &id, NOW - 89 * MS_PER_DAY);

    let report = retention::sweep(&mut db, &root, NOW, SweepMode::DryRun).unwrap();

    assert_eq!(report.plan.evictions.len(), 1);
    assert_eq!(report.bytes_reclaimed, 0, "a dry run reclaimed bytes");
    assert!(report.would_delete() > 0);

    let rows = db.audio_inventory().unwrap();
    assert_eq!(rows[0].audio.len(), 2, "a dry run retired the rows");
    for f in &rows[0].audio {
        assert!(
            root.join(&f.rel_path).exists(),
            "a dry run deleted {}",
            f.rel_path
        );
        assert!(
            report.render().contains(&f.rel_path),
            "the report does not name {}:\n{}",
            f.rel_path,
            report.render()
        );
    }
    assert!(report.render().contains("nothing was deleted"));

    // Deciding the same thing twice must give the same answer, or the preview
    // is not a preview.
    let again = retention::sweep(&mut db, &root, NOW, SweepMode::DryRun).unwrap();
    assert_eq!(again.plan, report.plan);
}

#[test]
fn applying_a_sweep_unlinks_the_audio_marks_the_rows_and_leaves_the_transcript_alone() {
    let root = tmpdir("apply");
    let mut db = db_at(&root);
    let id = seeded_meeting(&mut db, &root, "old", NOW - 90 * MS_PER_DAY, 4 * MIB, true);
    transcript_ready_at(&mut db, &id, NOW - 89 * MS_PER_DAY);
    let paths: Vec<String> = db.audio_inventory().unwrap()[0]
        .audio
        .iter()
        .map(|f| f.rel_path.clone())
        .collect();
    let text_before = db.audio_inventory().unwrap()[0].transcript_bytes;
    assert!(text_before > 0);

    let report = retention::sweep(&mut db, &root, NOW, SweepMode::Apply).unwrap();

    assert_eq!(report.bytes_reclaimed, 4 * MIB);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    for rel in &paths {
        assert!(!root.join(rel).exists(), "{rel} survived the sweep");
    }

    let rows = db.audio_inventory().unwrap();
    assert!(rows[0].audio.is_empty(), "the rows were not retired");
    assert_eq!(
        rows[0].transcript_bytes, text_before,
        "§9.5: transcripts are never subject to retention"
    );
    let state: String = db
        .conn()
        .query_row(
            "SELECT state FROM recordings WHERE meeting_id = ?1 LIMIT 1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "deleted", "the row was dropped instead of marked");

    // Idempotent: running it again finds nothing left to do and does not fail
    // on the files it already removed.
    let again = retention::sweep(&mut db, &root, NOW, SweepMode::Apply).unwrap();
    assert!(again.plan.evictions.is_empty());
    assert!(again.errors.is_empty());
}

#[test]
fn a_sweep_refreshes_the_purge_deadline_so_the_library_can_say_when_audio_goes() {
    let root = tmpdir("deadline");
    let mut db = db_at(&root);
    let id = seeded_meeting(&mut db, &root, "young", NOW - MS_PER_DAY, MIB, true);
    transcript_ready_at(&mut db, &id, NOW - MS_PER_DAY);

    retention::sweep(&mut db, &root, NOW, SweepMode::DryRun).unwrap();
    let deadline = db.audio_inventory().unwrap()[0].purge_after_ms.unwrap();
    assert_eq!(deadline as u64, NOW - MS_PER_DAY + 30 * MS_PER_DAY);

    // `forever` has no deadline, and a stale date left on screen would be a
    // lie about the one setting the user chose specifically to be told the
    // truth about.
    db.set_retain_audio(&id, "forever", None).unwrap();
    retention::sweep(&mut db, &root, NOW, SweepMode::DryRun).unwrap();
    assert_eq!(db.audio_inventory().unwrap()[0].purge_after_ms, None);
}

#[test]
fn the_budget_comes_from_the_library_and_survives_a_restart() {
    let root = tmpdir("settings");
    let want = fotw_pipeline::retention::RetentionSettings {
        default_days: 7,
        budget_bytes: 3 * 1024 * 1024 * 1024,
    };
    {
        let mut db = db_at(&root);
        assert_eq!(
            retention::settings(&db),
            fotw_pipeline::retention::RetentionSettings::default(),
            "an unconfigured library must fall back to the §9.5 defaults"
        );
        retention::set_settings(&mut db, &want).unwrap();
    }
    let db = db_at(&root);
    assert_eq!(retention::settings(&db), want);
}

#[test]
fn a_corrupt_settings_value_falls_back_to_the_defaults_instead_of_disabling_retention() {
    // Guessing wrong here in the other direction — refusing to sweep — fills
    // the disk silently, which is the failure this whole feature exists to
    // prevent.
    let root = tmpdir("corrupt-settings");
    let mut db = db_at(&root);
    db.put_setting("retention", "not json at all").unwrap();
    assert_eq!(
        retention::settings(&db),
        fotw_pipeline::retention::RetentionSettings::default()
    );
}

// ------------------------------------------------------------------ scheduling

#[test]
fn the_scheduler_runs_on_start_and_then_once_an_interval() {
    // Issue #41: "background sweeper on app start and hourly".
    let mut s = Schedule::hourly();
    let hour = 3_600_000;

    assert_eq!(s.poll(NOW, false), Tick::Run, "it must run at startup");
    assert!(matches!(s.poll(NOW + 1, false), Tick::Waiting { .. }));
    assert!(matches!(
        s.poll(NOW + hour - 1, false),
        Tick::Waiting { .. }
    ));
    assert_eq!(s.poll(NOW + hour, false), Tick::Run);
    assert!(matches!(
        s.poll(NOW + hour + 1, false),
        Tick::Waiting { .. }
    ));
    assert_eq!(s.poll(NOW + 2 * hour, false), Tick::Run);
}

#[test]
fn the_scheduler_never_sweeps_while_a_recording_is_in_flight() {
    // Competing for disk I/O with a live capture is how buffers get dropped,
    // and this project spent weeks eliminating dropped buffers. The sweep can
    // always wait; the meeting cannot.
    let mut s = Schedule::hourly();
    let hour = 3_600_000;

    assert_eq!(s.poll(NOW, true), Tick::HeldForRecording);
    assert_eq!(
        s.poll(NOW + hour, true),
        Tick::HeldForRecording,
        "a long meeting must not eventually be overridden"
    );
    // …and the deferral does not consume the turn: the moment the recording
    // ends, the sweep that was due runs.
    assert_eq!(s.poll(NOW + hour + 1, false), Tick::Run);
}

#[test]
fn a_live_session_on_disk_counts_as_a_recording_and_a_crashed_one_stops_counting() {
    let root = tmpdir("live-gate");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    assert!(
        !retention::recording_in_flight(&sessions, NOW),
        "an empty sessions directory is not a recording"
    );

    let mut wal = SessionWal::create(&sessions, RATE, 1).unwrap();
    wal.write_system(&tone(440.0, 0.2)).unwrap();
    wal.flush().unwrap();
    let dir = wal.dir().to_path_buf();

    let mtime = std::fs::metadata(dir.join("system.pcm"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    assert!(
        retention::recording_in_flight(&sessions, mtime),
        "an unfinalized session being written to is a live recording"
    );

    // A session that crashed stays unfinalized forever. If that blocked the
    // sweeper permanently, one crash would disable retention for good — so
    // the gate is "unfinalized AND recently written", not "unfinalized".
    assert!(
        !retention::recording_in_flight(&sessions, mtime + LIVE_SESSION_WINDOW_MS + 1),
        "a stale crashed session blocked the sweeper forever"
    );

    // And a cleanly finished one never blocks it at all.
    wal.finalize().unwrap();
    assert!(!retention::recording_in_flight(&sessions, mtime));
}
