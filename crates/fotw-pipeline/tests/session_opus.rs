//! Finalizing a session into the archival form (§9.5, §5.4).
//!
//! During a meeting the session directory holds headerless PCM, because the
//! file length *is* the length and nothing has to be rewritten on close. After
//! the meeting that same property is a liability: 32 kB/s per leg, 173 MB for
//! a 45-minute meeting, and no user is going to accept that twice a day. The
//! finalize path transcodes both legs to Opus and only then unlinks the PCM.
//!
//! Every assertion about the transcode decodes the result. "The file exists"
//! would pass against a writer that emitted two Ogg header pages and stopped,
//! and it would equally pass against one that wrote the *microphone* into
//! `system.opus` — which is why the two legs carry different tones here.

use std::f32::consts::TAU;

use fotw_pipeline::opus::decode_ogg_opus_file;
use fotw_pipeline::wal::{SessionState, SessionWal, discard_pcm, encode_session};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fotw-session-opus-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tone(hz: f32, secs: f32, amp: f32, rate: u32) -> Vec<f32> {
    let n = (rate as f32 * secs) as usize;
    (0..n)
        .map(|i| amp * (TAU * hz * i as f32 / rate as f32).sin())
        .collect()
}

fn goertzel(x: &[f32], hz: f32, rate: u32) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let coeff = 2.0 * (TAU * hz / rate as f32).cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / x.len() as f32
}

const CANDIDATES: [f32; 5] = [220.0, 440.0, 660.0, 880.0, 1_760.0];

/// Assert `x` is a real tone at `hz`.
///
/// Not `assert_eq!(dominant(..), hz)`: on a window of decoded silence the
/// noise floor still has a peak somewhere, and it lands on the right candidate
/// often enough that an encoder emitting nothing would pass intermittently.
/// The peak has to beat its neighbours by an order of magnitude and the window
/// has to contain audible energy.
#[track_caller]
fn assert_tone(x: &[f32], rate: u32, hz: f32, what: &str) {
    let (peak, energy) = CANDIDATES
        .iter()
        .map(|&f| (f, goertzel(x, f, rate)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap();
    let runner_up = CANDIDATES
        .iter()
        .filter(|&&f| f != hz)
        .map(|&f| goertzel(x, f, rate))
        .fold(0.0f32, f32::max);
    let rms = (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt();

    assert_eq!(
        peak, hz,
        "{what}: dominant frequency is {peak} Hz, not {hz} Hz"
    );
    assert!(
        energy > runner_up * 10.0,
        "{what}: {hz} Hz energy {energy:.6} barely beats {runner_up:.6}; this is noise"
    );
    assert!(rms > 0.05, "{what}: RMS {rms:.5} — there is no audio here");
}

#[test]
fn finalizing_archives_both_legs_as_opus_and_then_drops_the_pcm() {
    let root = tmpdir("archive");
    let rate = 16_000u32;
    let mut wal = SessionWal::create(&root, rate, 1).unwrap();

    // Different tones per leg. If the transcode mixed, swapped, or duplicated
    // them, the frequency assertions below catch it — a size check never
    // would.
    let system = tone(440.0, 3.0, 0.5, rate);
    let mic = tone(880.0, 3.0, 0.5, rate);
    wal.write_system(&system).unwrap();
    wal.write_mic(&mic).unwrap();

    let pcm_expected = system.len() as u64 * 2;
    let (dir, encoded) = wal.finalize_and_encode().unwrap();

    // The PCM is gone and the Opus is there. Order matters in the
    // implementation; here only the end state is observable.
    assert!(
        !dir.join("system.pcm").exists(),
        "raw PCM was not reclaimed"
    );
    assert!(!dir.join("mic.pcm").exists(), "raw PCM was not reclaimed");
    assert!(dir.join("system.opus").exists());
    assert!(dir.join("mic.opus").exists());

    assert_eq!(encoded.system.pcm_bytes, pcm_expected);
    assert_eq!(encoded.mic.pcm_bytes, pcm_expected);

    for (file, hz) in [("system.opus", 440.0f32), ("mic.opus", 880.0f32)] {
        let out = decode_ogg_opus_file(dir.join(file), rate).unwrap();
        let d = out.duration_ms();
        assert!(
            (2_950..=3_050).contains(&d),
            "{file} decoded to {d} ms of a 3,000 ms session"
        );
        let body = &out.samples[rate as usize / 20..];
        assert_tone(body, rate, hz, &format!("{file}'s leg"));
        assert!(
            out.rms(rate as usize / 20, out.samples.len()) > 0.25,
            "{file} decoded to something far quieter than what went in"
        );
    }

    // The ratio is the entire justification for the exercise.
    let ratio = encoded.compression_ratio();
    assert!(
        ratio > 8.0,
        "Opus was only {ratio:.1}x smaller than the PCM it replaced"
    );
    println!(
        "session compression: {} B PCM -> {} B Opus = {ratio:.2}x",
        encoded.pcm_bytes(),
        encoded.opus_bytes()
    );

    // And the manifest now says which artifact is authoritative, so a reader
    // that finds no `.pcm` knows that is by design rather than by loss.
    let state = SessionState::read(&dir).unwrap();
    assert!(state.is_finalized());
    let m = state
        .manifest
        .encoded
        .expect("manifest must record the encode");
    assert_eq!(m, encoded);
    assert!(m.system.duration_ms >= 2_950);
}

#[test]
fn the_pcm_is_never_unlinked_until_the_manifest_records_an_encode() {
    // The one irreversible mistake available here. `discard_pcm` is the only
    // thing in the crate that deletes the crash invariant, so it refuses
    // unless something else provably holds the meeting.
    let root = tmpdir("guard");
    let mut wal = SessionWal::create(&root, 16_000, 1).unwrap();
    wal.write_system(&tone(440.0, 0.5, 0.4, 16_000)).unwrap();
    let dir = wal.finalize().unwrap();

    let err = discard_pcm(&dir).expect_err("unlinking un-encoded PCM must be refused");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        dir.join("system.pcm").exists(),
        "the PCM was deleted despite the refusal"
    );

    // With an encode on record it goes through, and reports what it freed.
    // Only the system leg has audio here; the empty mic leg must still yield a
    // valid (silent) Opus file rather than an error or a missing artifact,
    // because a mic-denied meeting is an ordinary meeting.
    encode_session(&dir).unwrap();
    let mic = decode_ogg_opus_file(dir.join("mic.opus"), 16_000).unwrap();
    assert_eq!(mic.duration_ms(), 0);
    assert!(!mic.truncated);

    let freed = discard_pcm(&dir).unwrap();
    assert_eq!(freed, 8_000 * 2, "0.5 s of 16 kHz i16 is 16 kB");
    assert!(!dir.join("system.pcm").exists());

    // Idempotent: a second call on an already-swept session is not an error.
    assert_eq!(discard_pcm(&dir).unwrap(), 0);
}

#[test]
fn a_session_recovered_from_a_crash_can_still_be_archived() {
    // A meeting that ended in `kill -9` has no `ended_at_ms` and a possibly
    // torn PCM tail. It is still a meeting, and it still has to be archivable
    // without being replayed first.
    let root = tmpdir("recovered");
    let rate = 16_000u32;
    let mut wal = SessionWal::create(&root, rate, 1).unwrap();
    wal.write_system(&tone(440.0, 2.0, 0.5, rate)).unwrap();
    wal.write_mic(&tone(880.0, 2.0, 0.5, rate)).unwrap();
    wal.flush().unwrap();
    let dir = wal.dir().to_path_buf();
    std::mem::forget(wal); // hard kill: no finalize

    // Chop a byte off, exactly as a kill mid-write would.
    let p = dir.join("system.pcm");
    let len = std::fs::metadata(&p).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .unwrap()
        .set_len(len - 1)
        .unwrap();

    let encoded = encode_session(&dir).unwrap();
    let out = decode_ogg_opus_file(dir.join("system.opus"), rate).unwrap();
    let d = out.duration_ms();
    assert!(
        (1_950..=2_050).contains(&d),
        "a torn trailing sample cost {d} ms of a 2,000 ms recording"
    );
    assert_tone(&out.samples[800..], rate, 440.0, "the recovered system leg");
    assert!(encoded.system.opus_bytes > 0);

    // Still not finalized — archiving is not the same event as closing, and
    // conflating them would make a recovered session stop being offered for
    // recovery before the user had seen it.
    let state = SessionState::read(&dir).unwrap();
    assert!(!state.is_finalized());
    assert!(state.manifest.encoded.is_some());
}

#[test]
fn a_capture_rate_libopus_cannot_take_is_resampled_rather_than_refused() {
    // libopus takes 8/12/16/24/48 kHz. A session recovered from a 44.1 kHz
    // capture — the mic's normal rate before CAP-07's conversion runs — must
    // still be archivable, and must not come back 9% slow, which is what
    // encoding 44.1 kHz samples as if they were 48 kHz produces.
    let root = tmpdir("resample");
    let rate = 44_100u32;
    let mut wal = SessionWal::create(&root, rate, 1).unwrap();
    wal.write_system(&tone(440.0, 3.0, 0.5, rate)).unwrap();
    wal.write_mic(&tone(440.0, 3.0, 0.5, rate)).unwrap();
    let (dir, encoded) = wal.finalize_and_encode().unwrap();

    assert_eq!(
        encoded.system.sample_rate_hz, 16_000,
        "an unsupported rate must be resampled to the pipeline's own rate"
    );

    let out = decode_ogg_opus_file(dir.join("system.opus"), 16_000).unwrap();
    let d = out.duration_ms();
    assert!(
        (2_900..=3_100).contains(&d),
        "3.0 s at 44.1 kHz came back as {d} ms; the rate conversion is wrong"
    );
    // Pitch is the thing a rate mistake destroys, and it is audible.
    assert_tone(
        &out.samples[800..],
        16_000,
        440.0,
        "a 44.1 kHz session resampled for the encoder",
    );
}

#[test]
fn a_multichannel_session_is_downmixed_to_the_mono_stream_9_5_specifies() {
    let root = tmpdir("stereo");
    let rate = 48_000u32;
    let mut wal = SessionWal::create(&root, rate, 2).unwrap();

    // Interleave the same tone into both channels. Averaged (not summed), so
    // the level must come back unchanged rather than clipped — the failure
    // mode `Downmixer::to_mono` exists to avoid.
    let mono = tone(440.0, 2.0, 0.5, rate);
    let mut stereo = Vec::with_capacity(mono.len() * 2);
    for s in &mono {
        stereo.push(*s);
        stereo.push(*s);
    }
    wal.write_system(&stereo).unwrap();
    wal.write_mic(&stereo).unwrap();
    let (dir, encoded) = wal.finalize_and_encode().unwrap();

    let out = decode_ogg_opus_file(dir.join("system.opus"), rate).unwrap();
    assert_eq!(out.channels, 1, "§9.5 specifies mono Opus streams");
    let d = out.duration_ms();
    assert!(
        (1_950..=2_050).contains(&d),
        "a stereo session decoded to {d} ms of a 2,000 ms recording — the \
         frame arithmetic treated interleaved samples as frames"
    );
    assert_tone(
        &out.samples[rate as usize / 20..],
        rate,
        440.0,
        "a downmixed stereo session",
    );
    let rms = out.rms(rate as usize / 20, out.samples.len());
    assert!(
        (0.25..=0.45).contains(&rms),
        "downmixed RMS {rms:.3}; averaging two identical channels must not \
         change the level"
    );

    // Stereo PCM is twice the bytes for the same audio, so the ratio against
    // a mono Opus track is roughly twice as good. 48 kHz stereo i16 is the
    // 11.5 MB/minute per track that §9.5's table opens with.
    let ratio = encoded.system.compression_ratio();
    assert!(ratio > 16.0);
    println!(
        "48 kHz stereo i16 -> 16 kHz-class mono Opus: {} B -> {} B = {ratio:.1}x",
        encoded.system.pcm_bytes, encoded.system.opus_bytes
    );
}
