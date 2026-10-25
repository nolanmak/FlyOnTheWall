//! Clock ownership and durable-prefix evidence use temporary files, not capture.
use fotw_pipeline::{capture_clock::CaptureOwner, wal::SessionWal};
use std::io::Write;

#[test]
fn live_writer_and_recovery_are_exclusive_until_actual_owner_drop() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.checkpoint_capture(250, true).unwrap();
    let dir = wal.dir().to_owned();
    assert!(
        CaptureOwner::recover(&dir).is_err(),
        "a live writer is not recoverable"
    );
    drop(wal);
    let owner = CaptureOwner::recover(&dir).unwrap();
    assert!(
        CaptureOwner::recover(&dir).is_err(),
        "only one recovery owner may proceed"
    );
    drop(owner);
    assert!(CaptureOwner::recover(&dir).is_ok());
}
#[test]
fn first_stop_survives_later_drain_and_finalization_without_changing_the_clock() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    let start = wal.manifest().started_at_ms;
    wal.write_system(&vec![0.25; 1600]).unwrap();
    wal.checkpoint_capture(250, true).unwrap();
    wal.write_mic(&[0.5; 160]).unwrap();
    wal.checkpoint_capture(9999, true).unwrap();
    let dir = wal.finalize().unwrap();
    let snapshot = CaptureOwner::recover(&dir).unwrap().snapshot().unwrap();
    assert_eq!(snapshot.frozen_capture_clock(), Some((start, start + 250)));
    assert_eq!(snapshot.checkpoint.system_bytes, 3200);
    assert_eq!(snapshot.checkpoint.mic_bytes, 320);
}
#[test]
fn audio_after_a_checkpoint_is_retained_and_cannot_claim_a_frozen_complete_clock() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.write_system(&[0.25; 160]).unwrap();
    wal.checkpoint_capture(10, true).unwrap();
    wal.write_system(&[0.5; 160]).unwrap();
    wal.flush().unwrap();
    let dir = wal.dir().to_owned();
    drop(wal);
    let snapshot = CaptureOwner::recover(&dir).unwrap().snapshot().unwrap();
    assert_eq!(snapshot.system_tail_bytes, 320);
    assert_eq!(snapshot.frozen_capture_clock(), None);
    assert_eq!(
        std::fs::metadata(dir.join("system.pcm")).unwrap().len(),
        640
    );
}
#[test]
fn foreign_clock_and_truncated_checkpointed_audio_are_refused() {
    for foreign in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
        wal.write_system(&[0.25; 160]).unwrap();
        wal.checkpoint_capture(10, false).unwrap();
        let dir = wal.dir().to_owned();
        drop(wal);
        if foreign {
            let path = dir.join("capture-clock.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            value["session_id"] = "another-recording".into();
            std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .open(dir.join("system.pcm"))
                .unwrap()
                .set_len(2)
                .unwrap();
        }
        assert!(CaptureOwner::recover(&dir).unwrap().snapshot().is_err());
    }
}
#[test]
fn interrupted_prefix_has_original_observation_but_no_stop_proof_and_no_regression() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.checkpoint_capture(200, false).unwrap();
    assert!(
        wal.checkpoint_capture(100, false).is_err(),
        "observed capture clock cannot regress"
    );
    let dir = wal.dir().to_owned();
    drop(wal);
    let snapshot = CaptureOwner::recover(&dir).unwrap().snapshot().unwrap();
    assert_eq!(snapshot.checkpoint.elapsed_ms, 200);
    assert_eq!(snapshot.frozen_capture_clock(), None);
    assert!(snapshot.checkpoint.started_at_ms > 0);
}
#[test]
fn torn_clock_is_not_repaired_or_accepted_as_a_complete_record() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.checkpoint_capture(1, false).unwrap();
    let dir = wal.dir().to_owned();
    drop(wal);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("capture-clock.json"))
        .unwrap();
    file.write_all(b"broken").unwrap();
    drop(file);
    assert!(CaptureOwner::recover(&dir).unwrap().snapshot().is_err());
}

#[test]
fn legacy_missing_lease_is_not_manufactured_and_overflow_keeps_prior_clock() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.checkpoint_capture(50, false).unwrap();
    assert!(wal.checkpoint_capture(u64::MAX, true).is_err());
    let dir = wal.dir().to_owned();
    drop(wal);
    assert_eq!(
        CaptureOwner::recover(&dir)
            .unwrap()
            .snapshot()
            .unwrap()
            .checkpoint
            .elapsed_ms,
        50
    );
    std::fs::remove_file(dir.join("capture-owner.lock")).unwrap();
    assert!(CaptureOwner::recover(&dir).is_err());
    assert!(!dir.join("capture-owner.lock").exists());
}

#[test]
#[ignore = "private child process for capture-clock crash test"]
fn capture_crash_child() {
    let root = std::path::PathBuf::from(std::env::var_os("FOTW_CLOCK_CRASH_ROOT").unwrap());
    let mut wal = SessionWal::create(&root, 16000, 1).unwrap();
    wal.write_system(&[0.25; 1600]).unwrap();
    wal.checkpoint_capture(125, false).unwrap();
    std::fs::write(root.join("ready.tmp"), wal.dir().to_str().unwrap()).unwrap();
    std::fs::rename(root.join("ready.tmp"), root.join("ready")).unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn hard_kill_releases_owner_and_preserves_original_synced_prefix() {
    use std::{
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "capture_crash_child"])
            .env("FOTW_CLOCK_CRASH_ROOT", root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.path().join("ready").exists() && Instant::now() < deadline {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before sync"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let dir = std::path::PathBuf::from(
        std::fs::read_to_string(root.path().join("ready")).expect("child sync barrier"),
    );
    assert!(CaptureOwner::recover(&dir).is_err());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let snapshot = CaptureOwner::recover(&dir).unwrap().snapshot().unwrap();
    assert_eq!(snapshot.checkpoint.elapsed_ms, 125);
    assert_eq!(snapshot.checkpoint.system_bytes, 3200);
    assert_eq!(snapshot.system_tail_bytes, 0);
    assert_eq!(snapshot.frozen_capture_clock(), None);
}

#[test]
fn unsupported_or_foreign_clock_metadata_cannot_certify_audio() {
    for field in [
        "schema",
        "started_at_ms",
        "elapsed_ms",
        "system_format",
        "mic_format",
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
        wal.checkpoint_capture(10, true).unwrap();
        let dir = wal.dir().to_owned();
        drop(wal);
        let path = dir.join("capture-clock.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value[field] = match field {
            "schema" => 999.into(),
            "started_at_ms" => 0.into(),
            "elapsed_ms" => u64::MAX.into(),
            _ => serde_json::json!({"sample_rate_hz": 0, "channels": 1}),
        };
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            CaptureOwner::recover(&dir).unwrap().snapshot().is_err(),
            "{field}"
        );
    }
}

#[test]
fn stopped_clock_remains_frozen_when_later_checkpoint_says_live() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    let start = wal.manifest().started_at_ms;
    wal.checkpoint_capture(100, true).unwrap();
    wal.write_system(&[0.2; 320]).unwrap();
    wal.checkpoint_capture(5000, false).unwrap();
    let dir = wal.dir().to_owned();
    drop(wal);
    let snapshot = CaptureOwner::recover(&dir).unwrap().snapshot().unwrap();
    assert_eq!(snapshot.frozen_capture_clock(), Some((start, start + 100)));
    assert_eq!(snapshot.checkpoint.system_bytes, 640);
}

#[test]
fn first_stop_can_precede_a_later_live_observation_without_losing_drain() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    let start = wal.manifest().started_at_ms;
    wal.write_system(&[0.25; 320]).unwrap();
    wal.checkpoint_capture(250, false).unwrap();
    // The Stop caller sampled 200 before it was scheduled to publish Stop.
    // Meanwhile the pump observed 250 and is still preserving drain audio.
    wal.write_system(&[0.25; 320]).unwrap();
    wal.checkpoint_capture(200, true).unwrap();
    let dir = wal.finalize().unwrap();
    let snapshot = CaptureOwner::recover(&dir).unwrap().snapshot().unwrap();
    assert_eq!(snapshot.frozen_capture_clock(), Some((start, start + 200)));
    assert_eq!(snapshot.checkpoint.system_bytes, 1280);
}
