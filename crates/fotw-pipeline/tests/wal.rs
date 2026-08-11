//! The crash invariant: audio-to-disk survives, the transcript is derived.
//!
//! docs/REQUIREMENTS.md 5.4. The acceptance criterion is explicit and brutal:
//! `SIGKILL` at a random offset during a 90-minute run, then recover, must
//! yield audio of at least (kill_time - 5s). Panic hooks and signal handlers
//! flush, but **correctness must not depend on them** — so every test here
//! simulates a hard kill by simply not calling finalize.

use std::path::Path;

use fotw_pipeline::wal::{SessionState, SessionWal, SttRecord, recover};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fotw-wal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_session_writes_headerless_pcm_for_both_legs() {
    let root = tmpdir("two-legs");
    let mut wal = SessionWal::create(&root, 16_000, 1).unwrap();

    wal.write_system(&[0.5f32; 160]).unwrap();
    wal.write_mic(&[0.25f32; 160]).unwrap();
    wal.flush().unwrap();

    // Headerless on purpose: a RIFF header has to be rewritten on close with
    // the final length, and a hard kill leaves it wrong. Raw PCM plus a
    // manifest sidesteps the whole class of bug.
    let sys = wal.dir().join("system.pcm");
    let mic = wal.dir().join("mic.pcm");
    assert_eq!(std::fs::metadata(&sys).unwrap().len(), 160 * 2);
    assert_eq!(std::fs::metadata(&mic).unwrap().len(), 160 * 2);

    // Two logical tracks, never pre-mixed — seam rule 2, and what makes
    // "me vs them" free.
    assert_ne!(std::fs::read(&sys).unwrap(), std::fs::read(&mic).unwrap());
}

#[test]
fn an_unfinalized_session_is_recoverable_and_a_finalized_one_is_not_offered() {
    let root = tmpdir("unfinalized");

    let mut crashed = SessionWal::create(&root, 16_000, 1).unwrap();
    crashed.write_system(&[0.1f32; 1600]).unwrap();
    crashed.flush().unwrap();
    let crashed_dir = crashed.dir().to_path_buf();
    // Simulate SIGKILL: drop without finalize. No destructor may be relied on.
    std::mem::forget(crashed);

    let mut clean = SessionWal::create(&root, 16_000, 1).unwrap();
    clean.write_system(&[0.2f32; 1600]).unwrap();
    let clean_dir = clean.dir().to_path_buf();
    clean.finalize().unwrap();

    let recoverable: Vec<_> = recover(&root).unwrap();
    let dirs: Vec<&Path> = recoverable.iter().map(|s| s.dir.as_path()).collect();

    assert!(
        dirs.contains(&crashed_dir.as_path()),
        "a session with no ended_at must surface as recoverable"
    );
    assert!(
        !dirs.contains(&clean_dir.as_path()),
        "a cleanly finalized session must not be offered for recovery"
    );
}

#[test]
fn recovered_audio_is_within_five_seconds_of_the_kill_point() {
    let root = tmpdir("kill-point");
    let rate = 16_000u32;
    let mut wal = SessionWal::create(&root, rate, 1).unwrap();

    // 90 seconds of audio in 10 ms blocks, flushed on the normal cadence.
    let block = [0.3f32; 160];
    for _ in 0..9_000 {
        wal.write_system(&block).unwrap();
    }
    let dir = wal.dir().to_path_buf();
    std::mem::forget(wal); // hard kill

    let sessions = recover(&root).unwrap();
    let s = sessions.iter().find(|s| s.dir == dir).unwrap();

    let written_secs = s.system_frames as f64 / f64::from(rate);
    assert!(
        written_secs >= 90.0 - 5.0,
        "recovered {written_secs:.1}s of a 90s run; the spec allows losing at \
         most the last 5 seconds"
    );
}

#[test]
fn a_truncated_pcm_file_is_tolerated_rather_than_rejected() {
    let root = tmpdir("torn-pcm");
    let mut wal = SessionWal::create(&root, 16_000, 1).unwrap();
    wal.write_system(&[0.4f32; 1600]).unwrap();
    wal.flush().unwrap();
    let dir = wal.dir().to_path_buf();
    std::mem::forget(wal);

    // Kill mid-frame: chop one byte so the file is not a whole number of
    // samples. A reader that rejects this loses the entire meeting.
    let p = dir.join("system.pcm");
    let len = std::fs::metadata(&p).unwrap().len();
    let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
    f.set_len(len - 1).unwrap();
    drop(f);

    let sessions = recover(&root).unwrap();
    let s = sessions.iter().find(|x| x.dir == dir).unwrap();
    assert_eq!(
        s.system_frames, 1599,
        "the partial trailing frame is dropped and the rest is kept"
    );
}

#[test]
fn a_torn_final_jsonl_line_is_tolerated() {
    let root = tmpdir("torn-jsonl");
    let mut wal = SessionWal::create(&root, 16_000, 1).unwrap();
    for i in 0..5 {
        wal.append_stt(&SttRecord {
            seq: i,
            t0_ms: i * 1000,
            t1_ms: (i + 1) * 1000,
            text: format!("segment {i}"),
            audio_byte_offset: i * 32_000,
        })
        .unwrap();
    }
    wal.flush().unwrap();
    let dir = wal.dir().to_path_buf();
    std::mem::forget(wal);

    // Append a half-written record, exactly as a kill mid-write would leave.
    let p = dir.join("stt.jsonl");
    let mut txt = std::fs::read_to_string(&p).unwrap();
    txt.push_str("{\"seq\":5,\"t0_ms\":5000,\"te");
    std::fs::write(&p, txt).unwrap();

    let sessions = recover(&root).unwrap();
    let s = sessions.iter().find(|x| x.dir == dir).unwrap();
    assert_eq!(
        s.stt.len(),
        5,
        "the five complete records survive and the torn one is discarded"
    );
    assert_eq!(s.stt[4].text, "segment 4");
}

#[test]
fn the_manifest_records_gap_markers_across_a_device_rebuild() {
    let root = tmpdir("gaps");
    let mut wal = SessionWal::create(&root, 16_000, 1).unwrap();
    wal.write_system(&[0.1f32; 160]).unwrap();
    // AirPods connect: the tap is torn down and rebuilt, and the audio lost
    // in between must be recorded rather than silently closing the seam.
    wal.mark_gap(1_000, 1_250, "default output device changed")
        .unwrap();
    wal.write_system(&[0.1f32; 160]).unwrap();
    let dir = wal.finalize().unwrap();

    let s = SessionState::read(&dir).unwrap();
    assert_eq!(s.manifest.gaps.len(), 1);
    assert_eq!(s.manifest.gaps[0].duration_ms(), 250);
    assert!(s.manifest.ended_at_ms.is_some());
}
