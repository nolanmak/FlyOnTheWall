//! Durable canonical audio, independent of provider reconnect ring capacity.
use fotw_pipeline::replay::{MAX_READ_SAMPLES, ReplayReader, ReplaySource, ReplayWriter};
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("fotw-replay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_prefix_projection_ack_never_authorizes_whole_source_retirement() {
    let fixture = Fixture::new("prefix-projection-scope");
    let mut writer = ReplayWriter::create(&fixture.0, "s", ReplaySource::System, 11).unwrap();
    writer.append(&[1, 2, 3, 4]).unwrap();
    writer.sync().unwrap();
    let journal = fixture.0.join("system.stt-coverage.jsonl");
    std::fs::write(&journal, b"{}\n{torn").unwrap();
    let mut reader = ReplayReader::open(&fixture.0, "s", ReplaySource::System).unwrap();
    assert!(
        reader.mark_prefix_projected(13, 4).is_err(),
        "a torn record is not a valid projection boundary"
    );
    reader.mark_prefix_projected(13, 3).unwrap();
    assert!(reader.is_prefix_projected(13, 3).unwrap());
    assert!(!reader.is_prefix_projected(14, 3).unwrap());
    assert!(!reader.is_complete().unwrap());
    assert!(!reader.is_projected().unwrap());
    assert!(!fixture.0.join("system.stt-sealed.json").exists());
    assert!(!fixture.0.join("system.stt-complete.json").exists());
    std::fs::write(&journal, b"{}\n{}\n").unwrap();
    assert!(
        !reader.is_prefix_projected(13, 6).unwrap(),
        "new journal records require their own SQL acknowledgement"
    );
    writer.seal().unwrap();
    reader.mark_complete(15).unwrap();
    reader.mark_prefix_projected(15, 6).unwrap();
    std::fs::copy(
        fixture.0.join("system.stt-prefix-projected.json"),
        fixture.0.join("system.stt-projected.json"),
    )
    .unwrap();
    assert!(
        !reader.is_projected().unwrap_or(false),
        "prefix and whole-source proof formats must remain incompatible"
    );
}

#[test]
fn replays_an_outage_longer_than_the_provider_ring_from_original_sample_offsets() {
    let fixture = Fixture::new("long-outage");
    let anchor = 7_201_337;
    let mut writer =
        ReplayWriter::create(&fixture.0, "session-a", ReplaySource::System, anchor).unwrap();
    // 73 seconds: including zeros and repeated values, which must not be deduped.
    let input: Vec<i16> = (0..73 * 16_000).map(|n| (n % 31) as i16 - 15).collect();
    for chunk in input.chunks(997) {
        writer.append(chunk).unwrap();
    }
    writer.sync().unwrap();
    drop(writer); // reopen as a separate process would, with no in-memory ring.
    let mut reader = ReplayReader::open(&fixture.0, "session-a", ReplaySource::System).unwrap();
    let committed = 11 * 16_000 + 19;
    let mut position = anchor + committed as u64;
    let mut recovered = Vec::new();
    loop {
        let chunk = reader.read_from(position, 2_771).unwrap();
        if chunk.is_empty() {
            break;
        }
        position += chunk.len() as u64;
        recovered.extend(chunk);
    }
    assert_eq!(recovered, input[committed..]);
    assert_eq!(position, anchor + input.len() as u64);
    assert_eq!(reader.first_sample(), anchor);
}

#[test]
fn readers_do_not_move_the_live_append_position_and_tracks_keep_their_own_clock() {
    let fixture = Fixture::new("live");
    let mut system = ReplayWriter::create(&fixture.0, "s", ReplaySource::System, 11).unwrap();
    let mut mic = ReplayWriter::create(&fixture.0, "s", ReplaySource::Mic, 901).unwrap();
    system.append(&[1, 2, 3]).unwrap();
    mic.append(&[0, 0, -9]).unwrap();
    let mut one = ReplayReader::open(&fixture.0, "s", ReplaySource::System).unwrap();
    let mut two = ReplayReader::open(&fixture.0, "s", ReplaySource::System).unwrap();
    assert_eq!(one.read_from(12, 1).unwrap(), [2]);
    assert_eq!(two.read_from(11, 3).unwrap(), [1, 2, 3]);
    system.append(&[4, 5]).unwrap();
    assert_eq!(one.read_from(14, 10).unwrap(), [4, 5]);
    assert_eq!(two.read_from(11, 10).unwrap(), [1, 2, 3, 4, 5]);
    let mut mic_reader = ReplayReader::open(&fixture.0, "s", ReplaySource::Mic).unwrap();
    assert_eq!(mic_reader.read_from(901, 10).unwrap(), [0, 0, -9]);
    assert!(mic_reader.read_from(900, 1).is_err());
    assert!(one.read_from(17, 1).is_err()); // reject a hole, rather than silently rebase.
}

#[test]
fn refuses_overwrite_wrong_session_and_unbounded_catchup_allocations() {
    let fixture = Fixture::new("guards");
    let mut writer = ReplayWriter::create(&fixture.0, "original", ReplaySource::Mic, 0).unwrap();
    writer.append(&[i16::MIN, 0, i16::MAX]).unwrap();
    writer.sync().unwrap();
    drop(writer); // Exercise existing-file refusal independently of the live lock.
    let before: Vec<_> = std::fs::read_dir(&fixture.0)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            (path.clone(), std::fs::read(path).unwrap())
        })
        .collect();
    assert!(ReplayWriter::create(&fixture.0, "replacement", ReplaySource::Mic, 0).is_err());
    assert!(ReplayReader::open(&fixture.0, "replacement", ReplaySource::Mic).is_err());
    let mut reader = ReplayReader::open(&fixture.0, "original", ReplaySource::Mic).unwrap();
    assert!(reader.read_from(0, usize::MAX).is_err());
    assert!(reader.read_from(0, MAX_READ_SAMPLES + 1).is_err());
    assert!(reader.read_from(0, 0).is_err());
    assert_eq!(
        reader.read_from(0, MAX_READ_SAMPLES).unwrap(),
        [i16::MIN, 0, i16::MAX]
    );
    for (path, bytes) in before {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn rejects_sample_clock_overflow_before_writing_any_audio() {
    let fixture = Fixture::new("overflow");
    let mut writer =
        ReplayWriter::create(&fixture.0, "s", ReplaySource::System, u64::MAX - 1).unwrap();
    assert!(writer.append(&[1, 2]).is_err());
    assert_eq!(writer.end_sample(), u64::MAX - 1);
    let mut reader = ReplayReader::open(&fixture.0, "s", ReplaySource::System).unwrap();
    assert!(reader.read_from(u64::MAX - 1, 1).unwrap().is_empty());
}

#[test]
fn restart_repairs_only_a_torn_sample_and_refuses_a_second_live_writer() {
    use std::io::Write;
    let fixture = Fixture::new("restart");
    let mut writer = ReplayWriter::create(&fixture.0, "s", ReplaySource::Mic, 101).unwrap();
    writer.append(&[1, -2, 3]).unwrap();
    writer.sync().unwrap();
    assert!(ReplayWriter::resume(&fixture.0, "s", ReplaySource::Mic).is_err());
    drop(writer);
    let path = fixture.0.join("mic.stt16");
    // Simulate a crash halfway through a PCM16 sample. Existing complete
    // samples survive, and that byte must not become half of the next sample.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[0xab])
        .unwrap();
    let mut reader = ReplayReader::open(&fixture.0, "s", ReplaySource::Mic).unwrap();
    assert_eq!(reader.read_from(101, 10).unwrap(), [1, -2, 3]);
    let before = std::fs::read(&path).unwrap();
    assert!(ReplayWriter::resume(&fixture.0, "wrong-session", ReplaySource::Mic).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut resumed = ReplayWriter::resume(&fixture.0, "s", ReplaySource::Mic).unwrap();
    assert_eq!(resumed.end_sample(), 104);
    resumed.append(&[-4, 5]).unwrap();
    assert_eq!(resumed.sync().unwrap(), 106);
    assert_eq!(reader.read_from(101, 10).unwrap(), [1, -2, 3, -4, 5]);
}

#[test]
fn malformed_headers_fail_closed_without_rewriting_evidence() {
    use std::io::{Seek, SeekFrom, Write};
    for (name, offset, corrupt) in [
        ("magic", 0, vec![0; 8]),
        ("length", 8, u32::MAX.to_le_bytes().to_vec()),
        ("json", 12, vec![0; 10]),
    ] {
        let fixture = Fixture::new(name);
        let writer = ReplayWriter::create(&fixture.0, "s", ReplaySource::System, 0).unwrap();
        drop(writer);
        let path = fixture.0.join("system.stt16");
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&corrupt).unwrap();
        drop(file);
        let before = std::fs::read(&path).unwrap();
        assert!(ReplayReader::open(&fixture.0, "s", ReplaySource::System).is_err());
        assert!(ReplayWriter::resume(&fixture.0, "s", ReplaySource::System).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn a_checkpoint_can_sync_only_an_interval_that_is_actually_saved() {
    let fixture = Fixture::new("checkpoint");
    let mut writer = ReplayWriter::create(&fixture.0, "s", ReplaySource::System, 17).unwrap();
    writer.append(&[9, 0, -9]).unwrap();
    let mut reader = ReplayReader::open(&fixture.0, "s", ReplaySource::System).unwrap();
    // The handoff controller owns a reader, not the pump's writer. It needs
    // this barrier before journaling a provider commit and publishing text.
    assert!(reader.sync_through(16).is_err());
    assert!(reader.sync_through(21).is_err());
    reader.sync_through(19).unwrap();
    writer.append(&[10]).unwrap();
    reader.sync_through(21).unwrap();
    assert_eq!(reader.read_from(17, 10).unwrap(), [9, 0, -9, 10]);
}

// Invoked only by the hard-kill test, in a child test process. The parent
// kills it while it owns the writer; no Rust destructor can flush or unlock.
#[test]
#[ignore = "private child process for the crash recovery test"]
fn crash_process_fixture() {
    let path = PathBuf::from(std::env::var_os("FOTW_REPLAY_CRASH_FIXTURE").unwrap());
    let mut writer = ReplayWriter::create(&path, "killed", ReplaySource::System, 83).unwrap();
    writer.append(&[42; 32_019]).unwrap();
    writer.sync().unwrap();
    std::fs::write(path.join("synced"), b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn synced_audio_and_writer_ownership_survive_a_hard_process_kill() {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let fixture = Fixture::new("hard-kill");
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "crash_process_fixture"])
            .env("FOTW_REPLAY_CRASH_FIXTURE", &fixture.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture.0.join("synced").exists() && Instant::now() < deadline {
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("crash fixture exited before its sync barrier: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        fixture.0.join("synced").exists(),
        "child did not sync audio within deadline"
    );
    assert!(ReplayWriter::resume(&fixture.0, "killed", ReplaySource::System).is_err());
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    let mut reader = ReplayReader::open(&fixture.0, "killed", ReplaySource::System).unwrap();
    for (position, length) in [(83, 16_000), (16_083, 16_000), (32_083, 19)] {
        assert_eq!(
            reader.read_from(position, MAX_READ_SAMPLES).unwrap(),
            vec![42; length]
        );
    }
    let mut resumed = ReplayWriter::resume(&fixture.0, "killed", ReplaySource::System).unwrap();
    assert_eq!(resumed.end_sample(), 32_102);
    resumed.append(&[-42]).unwrap();
    assert_eq!(reader.read_from(32_102, 1).unwrap(), [-42]);
}

#[test]
fn sealed_input_survives_restart_and_refuses_any_further_append() {
    let fixture = Fixture::new("sealed-input");
    let mut writer =
        ReplayWriter::create(&fixture.0, "sealed-fixture", ReplaySource::Mic, 137 * 16).unwrap();
    writer.append(&[11, -12, 0, 13]).unwrap();
    let end = writer.seal().unwrap();
    assert!(writer.append(&[99]).is_err());
    assert_eq!(writer.seal().unwrap(), end);
    drop(writer);
    let mut recovered =
        ReplayWriter::resume(&fixture.0, "sealed-fixture", ReplaySource::Mic).unwrap();
    assert!(recovered.append(&[99]).is_err());
    let mut reader = ReplayReader::open(&fixture.0, "sealed-fixture", ReplaySource::Mic).unwrap();
    assert_eq!(reader.sealed_end().unwrap(), Some(end));
    assert_eq!(reader.read_from(137 * 16, 4).unwrap(), [11, -12, 0, 13]);
    assert!(
        !reader.is_complete().unwrap(),
        "sealed input is not an inference acknowledgement"
    );
}

#[test]
fn altered_sealed_audio_is_refused_without_truncating_or_acknowledging_it() {
    use std::io::Write;
    let fixture = Fixture::new("sealed-altered");
    let mut writer =
        ReplayWriter::create(&fixture.0, "sealed-fixture", ReplaySource::System, 0).unwrap();
    writer.append(&[1, 2, 3]).unwrap();
    writer.seal().unwrap();
    drop(writer);
    let path = fixture.0.join("system.stt16");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&[42]).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(ReplayWriter::resume(&fixture.0, "sealed-fixture", ReplaySource::System).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let reader = ReplayReader::open(&fixture.0, "sealed-fixture", ReplaySource::System).unwrap();
    assert!(reader.sealed_end().is_err());
    assert!(reader.is_complete().is_err());
}
