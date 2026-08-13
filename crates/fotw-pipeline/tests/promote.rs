//! Promotion: `sessions/<ulid>/` → `media/<yyyy>/<mm>/<meeting_id>/` (§9.2).
//!
//! This is the step that was missing entirely. Without it a session directory
//! is never retired, `media/` is never written, and the retention engine has
//! nothing to sweep — so it does not matter how correct the sweeper is.
//!
//! Every crash-safety claim here is tested by *stopping between the steps*
//! rather than asserted in a comment. The steps are public functions for
//! exactly that reason: a test can run one, inspect the disk, and then run the
//! resumable whole and prove it converges. The two properties being defended:
//!
//! 1. **A crash mid-promotion never loses the only copy of the audio.** The
//!    session directory — PCM included until the encode lands — survives every
//!    interruption up to the point where the media files provably exist.
//! 2. **A crash never leaves a half-written Opus file that later looks
//!    complete.** Bytes land under a `.part` name and reach the final name by
//!    `rename(2)`, so a partial file is never mistakable for a finished track.

use std::path::Path;

use fotw_pipeline::opus::decode_ogg_opus_file;
use fotw_pipeline::promote::{
    self, MEDIA_DIR, PromoteError, media_rel_dir, publish, retire, stamp,
};
use fotw_pipeline::wal::SessionWal;

const RATE: u32 = 16_000;
/// 2026-08-13T00:00:00Z, so the layout under test is `media/2026/08/…`.
const AUG_2026_MS: u64 = 1_786_579_200_000;

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "fotw-promote-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tone(hz: f32, secs: f32, rate: u32) -> Vec<f32> {
    let n = (rate as f32 * secs) as usize;
    (0..n)
        .map(|i| 0.5 * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin())
        .collect()
}

/// A finished, claimed session with two distinguishable legs.
///
/// Different tones per leg so a promotion that swapped, mixed or duplicated
/// them is caught by content rather than by file size.
fn finished_session(root: &Path, meeting_id: &str) -> std::path::PathBuf {
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut wal = SessionWal::create(&sessions, RATE, 1).unwrap();
    wal.write_system(&tone(440.0, 2.0, RATE)).unwrap();
    wal.write_mic(&tone(880.0, 2.0, RATE)).unwrap();
    let dir = wal.finalize().unwrap();
    promote::claim(&dir, meeting_id, AUG_2026_MS).unwrap();
    dir
}

fn media_dir(root: &Path, meeting_id: &str) -> std::path::PathBuf {
    root.join(media_rel_dir(AUG_2026_MS, meeting_id))
}

#[test]
fn the_media_layout_is_year_month_meeting_and_the_path_is_relative() {
    // §9.2's layout, and §9.7 invariant 5: what goes in the database must be
    // relative, so the library can be moved or restored on another machine.
    let rel = media_rel_dir(AUG_2026_MS, "mtg-1");
    assert_eq!(rel, Path::new("media/2026/08/mtg-1"));
    assert!(!rel.is_absolute());
    assert!(rel.starts_with(MEDIA_DIR));

    // A January date, because a month formatted without its leading zero
    // sorts wrongly and only shows up eleven months later.
    assert_eq!(
        media_rel_dir(1_767_225_600_000, "mtg-2"),
        Path::new("media/2026/01/mtg-2")
    );
}

#[test]
fn promoting_a_session_writes_both_opus_tracks_into_the_media_tree_and_retires_the_session() {
    let root = tmpdir("happy");
    let dir = finished_session(&root, "mtg-happy");

    let promoted = promote::promote(&dir, &root).unwrap();

    assert_eq!(promoted.meeting_id, "mtg-happy");
    assert_eq!(promoted.tracks.len(), 2);
    assert!(!dir.exists(), "the session directory was not retired");

    let media = media_dir(&root, "mtg-happy");
    for (channel, hz) in [("system", 440.0f32), ("mic", 880.0f32)] {
        let track = promoted
            .tracks
            .iter()
            .find(|t| t.channel == channel)
            .unwrap_or_else(|| panic!("no {channel} track"));

        // The recorded path is relative and resolves under the data root.
        let rel = Path::new(&track.rel_path);
        assert!(!rel.is_absolute(), "{} is absolute", track.rel_path);
        assert!(
            !track.rel_path.contains(".."),
            "{} escapes the root",
            track.rel_path
        );
        let abs = root.join(rel);
        assert_eq!(abs, media.join(format!("{channel}.opus")));
        assert!(abs.exists(), "{} was not written", abs.display());
        assert_eq!(abs.metadata().unwrap().len(), track.bytes);
        assert!(track.bytes > 0, "{channel} track is empty");

        // Content, not size: a promotion that wrote two Ogg headers and
        // stopped would pass a size check.
        let out = decode_ogg_opus_file(&abs, RATE).unwrap();
        assert!(
            (1_950..=2_050).contains(&out.duration_ms()),
            "{channel} decoded to {} ms of a 2,000 ms session",
            out.duration_ms()
        );
        assert!(!out.truncated, "{channel} ends in a torn page");
        assert!(
            dominant(&out.samples, RATE) == hz,
            "{channel} carries the wrong leg"
        );
    }
}

/// The strongest single assertion in this file: if promotion silently skipped
/// writing the Opus file, this is what fails.
#[test]
fn promotion_that_skipped_writing_the_opus_is_caught_by_decoding_it() {
    let root = tmpdir("decode");
    let dir = finished_session(&root, "mtg-decode");
    let promoted = promote::promote(&dir, &root).unwrap();

    for track in &promoted.tracks {
        let abs = root.join(&track.rel_path);
        let out = decode_ogg_opus_file(&abs, RATE).unwrap();
        assert!(
            out.packets > 90,
            "{} holds {} audio packets; 2 s at 20 ms frames is ~100",
            track.channel,
            out.packets
        );
        assert!(
            out.samples.len() > RATE as usize,
            "{} decoded to under a second of audio",
            track.channel
        );
    }
}

#[test]
fn a_crash_before_the_encode_leaves_the_pcm_as_the_only_copy_and_the_retry_still_works() {
    let root = tmpdir("crash-encode");
    let dir = finished_session(&root, "mtg-a");

    // The process dies the instant the session is finalized. Nothing has been
    // encoded and nothing has been published, so the PCM must still be there —
    // it is the only copy of the meeting.
    assert!(dir.join("system.pcm").exists());
    assert!(dir.join("mic.pcm").exists());
    assert!(!media_dir(&root, "mtg-a").exists());

    let promoted = promote::promote(&dir, &root).unwrap();
    assert_eq!(promoted.tracks.len(), 2);
    for t in &promoted.tracks {
        assert!(root.join(&t.rel_path).exists());
    }
}

#[test]
fn a_crash_after_the_encode_leaves_the_session_intact_and_the_retry_does_not_re_encode() {
    let root = tmpdir("crash-publish");
    let dir = finished_session(&root, "mtg-b");

    // Step one only, then "die".
    promote::encode(&dir).unwrap();
    assert!(
        dir.join("system.opus").exists() && dir.join("mic.opus").exists(),
        "the encode step produced nothing"
    );
    assert!(
        !media_dir(&root, "mtg-b").exists(),
        "media was written before the encode was recorded"
    );

    // The Opus in the session dir is the copy that must be reused rather than
    // rebuilt: stamp it with a recognisable mtime-independent marker by
    // remembering its bytes.
    let before = std::fs::read(dir.join("system.opus")).unwrap();

    let promoted = promote::promote(&dir, &root).unwrap();
    let after = std::fs::read(root.join(&promoted.tracks[0].rel_path)).unwrap();
    assert_eq!(
        before, after,
        "the retry re-encoded instead of reusing the finished Opus"
    );
    assert!(!dir.exists());
}

#[test]
fn a_failed_publish_never_leaves_a_half_written_opus_wearing_the_final_name() {
    let root = tmpdir("crash-mid-publish");
    let dir = finished_session(&root, "mtg-c");
    promote::encode(&dir).unwrap();

    // Make the destination unwritable, which is as close as a test can get to
    // "the process died with the copy half done" without racing it.
    let media = media_dir(&root, "mtg-c");
    std::fs::create_dir_all(&media).unwrap();
    let mut perms = std::fs::metadata(&media).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o555);
    }
    std::fs::set_permissions(&media, perms).unwrap();

    let err = promote::promote(&dir, &root).unwrap_err();
    assert!(
        matches!(err, PromoteError::Io { .. }),
        "expected an I/O failure, got {err:?}"
    );

    // Nothing wearing a final name. A `.part` may be there; that is the point
    // of the name.
    assert!(
        !media.join("system.opus").exists() && !media.join("mic.opus").exists(),
        "a partial write reached the final name"
    );
    // And the session — PCM included, since the PCM is only dropped once the
    // media provably exists — is untouched.
    assert!(
        dir.join("system.pcm").exists(),
        "the only copy was unlinked"
    );
    assert!(dir.join("system.opus").exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&media).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&media, perms).unwrap();
    }

    // The retry converges, and the result is a complete, decodable track.
    let promoted = promote::promote(&dir, &root).unwrap();
    let out = decode_ogg_opus_file(root.join(&promoted.tracks[0].rel_path), RATE).unwrap();
    assert!((1_950..=2_050).contains(&out.duration_ms()));
    assert!(!dir.exists());
}

#[test]
fn a_stale_part_file_is_never_mistaken_for_a_finished_track() {
    let root = tmpdir("stale-part");
    let dir = finished_session(&root, "mtg-d");
    let media = media_dir(&root, "mtg-d");
    std::fs::create_dir_all(&media).unwrap();
    // Exactly what a kill during the copy leaves behind: a truncated Ogg
    // stream under the in-progress name.
    std::fs::write(media.join("system.opus.part"), b"OggS-garbage").unwrap();

    let promoted = promote::promote(&dir, &root).unwrap();

    for t in &promoted.tracks {
        assert!(
            !t.rel_path.ends_with(".part"),
            "a .part file was recorded as a finished track: {}",
            t.rel_path
        );
    }
    assert!(
        !media.join("system.opus.part").exists(),
        "the stale .part survived the promotion that superseded it"
    );
    let out = decode_ogg_opus_file(media.join("system.opus"), RATE).unwrap();
    assert!((1_950..=2_050).contains(&out.duration_ms()));
}

#[test]
fn a_crash_after_publishing_but_before_retiring_finishes_on_the_next_run() {
    let root = tmpdir("crash-retire");
    let dir = finished_session(&root, "mtg-e");

    promote::encode(&dir).unwrap();
    let promoted = publish(&dir, &root).unwrap();
    stamp(&dir, &promoted).unwrap();
    // …and the process dies here, with the media written and the session
    // directory still on disk.
    assert!(dir.exists());

    let again = promote::promote(&dir, &root).unwrap();
    assert_eq!(again.tracks, promoted.tracks);
    assert!(!dir.exists(), "the interrupted retire never completed");
    for t in &again.tracks {
        assert!(root.join(&t.rel_path).exists());
    }
}

#[test]
fn retiring_refuses_while_the_media_it_would_replace_is_missing_or_truncated() {
    // The rule that makes the whole sequence safe: the session directory is
    // the only copy until the media provably is one. "Provably" means the file
    // is there and is the size that was recorded, not that a rename returned
    // Ok at some point in the past.
    let root = tmpdir("verify");
    let dir = finished_session(&root, "mtg-f");
    promote::encode(&dir).unwrap();
    let promoted = publish(&dir, &root).unwrap();
    stamp(&dir, &promoted).unwrap();

    let victim = root.join(&promoted.tracks[1].rel_path);
    std::fs::remove_file(&victim).unwrap();
    let err = retire(&dir, &root).unwrap_err();
    assert!(
        matches!(err, PromoteError::Unverified { .. }),
        "retire removed a session whose media was gone: {err:?}"
    );
    assert!(dir.exists(), "the only copy of the meeting was unlinked");

    // Truncation is the other half: a file that exists but is short is a
    // half-written file, and must be refused just as loudly.
    std::fs::write(&victim, b"OggS").unwrap();
    let err = retire(&dir, &root).unwrap_err();
    assert!(
        matches!(err, PromoteError::Unverified { .. }),
        "retire accepted a truncated track: {err:?}"
    );
    assert!(dir.exists());

    // And the resumable whole repairs it rather than getting stuck.
    promote::promote(&dir, &root).unwrap();
    assert!(!dir.exists());
    assert!(victim.exists());
}

#[test]
fn promotion_is_idempotent_and_a_second_run_on_a_retired_session_is_not_an_error() {
    let root = tmpdir("idempotent");
    let dir = finished_session(&root, "mtg-g");
    let first = promote::promote(&dir, &root).unwrap();

    // The session is gone; running again must say so rather than panicking or
    // — far worse — reporting success with no tracks.
    let err = promote::promote(&dir, &root).unwrap_err();
    assert!(matches!(err, PromoteError::NoSession { .. }), "{err:?}");

    for t in &first.tracks {
        assert!(root.join(&t.rel_path).exists());
    }
}

#[test]
fn an_unclaimed_session_is_refused_because_there_is_nowhere_to_put_it() {
    // The media path is keyed by meeting id. A session with no meeting id has
    // no destination, and guessing one would scatter audio the library can
    // never find again.
    let root = tmpdir("unclaimed");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut wal = SessionWal::create(&sessions, RATE, 1).unwrap();
    wal.write_system(&tone(440.0, 0.5, RATE)).unwrap();
    let dir = wal.finalize().unwrap();

    let err = promote::promote(&dir, &root).unwrap_err();
    assert!(matches!(err, PromoteError::Unclaimed { .. }), "{err:?}");
    assert!(dir.exists(), "an unclaimed session was destroyed");
}

#[test]
fn pending_finds_every_claimed_session_that_still_needs_promoting_and_resume_finishes_them() {
    let root = tmpdir("resume");
    let sessions = root.join("sessions");
    let a = finished_session(&root, "mtg-h1");
    let b = finished_session(&root, "mtg-h2");

    // One of them got as far as publishing before the crash; the other did
    // not. Both must be found.
    promote::encode(&a).unwrap();
    let p = publish(&a, &root).unwrap();
    stamp(&a, &p).unwrap();

    let mut pending = promote::pending(&sessions).unwrap();
    pending.sort();
    assert_eq!(pending, vec![a.clone(), b.clone()]);

    let done = promote::resume(&sessions, &root);
    assert_eq!(done.len(), 2);
    assert!(done.iter().all(std::result::Result::is_ok), "{done:?}");
    assert!(!a.exists() && !b.exists());
    assert!(promote::pending(&sessions).unwrap().is_empty());
}

#[test]
fn a_live_session_is_never_promoted_out_from_under_the_recorder() {
    // `pending` keys off `ended_at_ms`, whose absence is §5.4's recovery
    // signal. Promoting a session that is still being written would race the
    // pump for its own PCM.
    let root = tmpdir("live");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut wal = SessionWal::create(&sessions, RATE, 1).unwrap();
    wal.write_system(&tone(440.0, 0.5, RATE)).unwrap();
    wal.flush().unwrap();
    let dir = wal.dir().to_path_buf();
    promote::claim(&dir, "mtg-live", AUG_2026_MS).unwrap();

    assert!(
        promote::pending(&sessions).unwrap().is_empty(),
        "an unfinalized session was queued for promotion"
    );
    let err = promote::promote(&dir, &root).unwrap_err();
    assert!(matches!(err, PromoteError::Unfinalized { .. }), "{err:?}");
    assert!(dir.join("system.pcm").exists());
}

/// The strongest of the five candidate frequencies, for leg identification.
fn dominant(x: &[f32], rate: u32) -> f32 {
    const CANDIDATES: [f32; 4] = [220.0, 440.0, 880.0, 1_760.0];
    CANDIDATES
        .iter()
        .map(|&f| (f, goertzel(x, f, rate)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
        .0
}

fn goertzel(x: &[f32], hz: f32, rate: u32) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let coeff = 2.0 * (std::f32::consts::TAU * hz / rate as f32).cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / x.len() as f32
}
