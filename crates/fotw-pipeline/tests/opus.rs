//! Opus encoding and incremental Ogg muxing (docs/REQUIREMENTS.md §9.5,
//! CAP-10).
//!
//! # These tests decode
//!
//! "The `.opus` file exists and is non-empty" asserts approximately nothing: a
//! writer that emitted only the two Ogg header pages would pass it, and so
//! would one that encoded pure silence. Every test here that claims audio
//! survived encoding decodes the file back through libopus and measures the
//! **content** — the dominant frequency, the amplitude envelope over time, and
//! the duration.
//!
//! Opus is lossy and its first 6.5 ms are encoder lookahead, so sample
//! equality is not available and is not what matters. A 440 Hz tone in must
//! yield a signal whose energy is concentrated at 440 Hz out, at the right
//! level, for the right length of time. That is a property a broken encoder
//! cannot fake.

use std::f32::consts::TAU;
use std::io::Cursor;

use fotw_pipeline::opus::{
    BITRATE_BPS, DecodedOpus, FRAME_MS, OpusOggWriter, PAGE_INTERVAL_MS, decode_ogg_opus,
    decode_ogg_opus_file,
};

const RATE: u32 = 16_000;

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fotw-opus-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A pure tone at `hz`, `secs` long, at amplitude `amp`.
fn tone(hz: f32, secs: f32, amp: f32) -> Vec<f32> {
    let n = (RATE as f32 * secs) as usize;
    (0..n)
        .map(|i| amp * (TAU * hz * i as f32 / RATE as f32).sin())
        .collect()
}

/// Goertzel: the energy at exactly `hz` in `x`, normalised by window length.
///
/// A full FFT would work too, but Goertzel is six lines and needs no
/// dependency, and the question here is not "what is the spectrum" but "how
/// much is at this one frequency" — which is precisely what it answers.
fn goertzel(x: &[f32], hz: f32, rate: u32) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let k = TAU * hz / rate as f32;
    let coeff = 2.0 * k.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power.max(0.0).sqrt() / x.len() as f32
}

/// The frequency, from a set of candidates, carrying the most energy.
fn dominant(x: &[f32], rate: u32, candidates: &[f32]) -> (f32, f32) {
    candidates
        .iter()
        .map(|&f| (f, goertzel(x, f, rate)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
}

/// Assert that `x` is a real tone at `hz` — not merely that `hz` won a
/// contest among candidates.
///
/// The distinction matters. `dominant` on a window of decoded *silence* still
/// returns some frequency, because the noise floor is not exactly zero, and it
/// will occasionally return the right one by chance. An encoder that dropped
/// every sample on the floor therefore passes a bare `assert_eq!(dominant(..),
/// 440.0)` about one time in `candidates.len()`. Requiring the peak to
/// dominate its neighbours *and* the window to have real energy in it closes
/// that hole.
#[track_caller]
fn assert_tone(x: &[f32], rate: u32, hz: f32, what: &str) {
    let candidates = [110.0, 220.0, 330.0, 440.0, 550.0, 660.0, 880.0, 1_760.0];
    assert!(
        candidates.contains(&hz),
        "test bug: {hz} is not among the candidates"
    );
    let (peak, energy) = dominant(x, rate, &candidates);
    let runner_up = candidates
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
        "{what}: {hz} Hz energy {energy:.6} is not clearly above the next \
         candidate {runner_up:.6} — this window is noise, not a tone"
    );
    assert!(
        rms > 0.05,
        "{what}: the window has RMS {rms:.5}; there is no audio in it at all"
    );
}

/// Encode `pcm` through a real file and hand the bytes back.
///
/// Through a file rather than a `Vec` sink on purpose: the file path is the
/// one production uses, and it is the only one where the flush/`sync`
/// distinction that CAP-10 rests on is real.
fn encode(pcm: &[f32], rate: u32) -> Vec<u8> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    // Tests run in parallel threads of one process, so the name must be unique
    // per call, not per process.
    let dir = std::env::temp_dir().join(format!(
        "fotw-opus-enc-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.opus");
    let mut w = OpusOggWriter::create(&path, rate).unwrap();
    w.push_f32(pcm).unwrap();
    w.finish().unwrap();
    std::fs::read(&path).unwrap()
}

fn decode(bytes: &[u8]) -> DecodedOpus {
    decode_ogg_opus(Cursor::new(bytes.to_vec()), RATE).unwrap()
}

#[test]
fn a_440_hz_tone_survives_encoding_as_a_440_hz_tone() {
    // Three seconds is long enough that the encoder's rate control has settled
    // and short enough to stay a unit test.
    let pcm = tone(440.0, 3.0, 0.5);
    let bytes = encode(&pcm, RATE);
    let out = decode(&bytes);

    // Duration first: an encoder that dropped nine tenths of the audio could
    // still show a 440 Hz peak in what it kept.
    let d = out.duration_ms();
    assert!(
        (2_950..=3_050).contains(&d),
        "3.0 s of tone decoded to {d} ms; end-trimming or granule arithmetic is wrong"
    );

    // Skip the first 50 ms: the encoder converges over the first few frames
    // and the very start is lookahead the decoder has already trimmed.
    let body = &out.samples[RATE as usize / 20..];
    assert_tone(body, RATE, 440.0, "a 3 s round trip");

    // Amplitude is preserved to within the tolerance a 24 kbps codec earns.
    // RMS of a 0.5-amplitude sine is 0.354.
    let rms = out.rms(RATE as usize / 20, out.samples.len());
    assert!(
        (0.25..=0.45).contains(&rms),
        "decoded RMS {rms:.3} is nowhere near the 0.354 that went in"
    );
}

#[test]
fn the_amplitude_envelope_survives_encoding() {
    // Loud / silent / loud. A stuck encoder that emitted a constant signal —
    // or the same page over and over — passes a frequency test and fails this
    // one.
    let mut pcm = tone(440.0, 1.0, 0.6);
    pcm.extend(std::iter::repeat_n(0.0f32, RATE as usize));
    pcm.extend(tone(440.0, 1.0, 0.6));

    let out = decode(&encode(&pcm, RATE));
    let sec = RATE as usize;
    assert!(
        out.samples.len() >= 3 * sec - sec / 10,
        "audio was truncated"
    );

    // Sample the middle of each second, away from the codec's transitions.
    let loud_a = out.rms(sec / 4, sec * 3 / 4);
    let quiet = out.rms(sec + sec / 4, sec + sec * 3 / 4);
    let loud_b = out.rms(2 * sec + sec / 4, 2 * sec + sec * 3 / 4);

    assert!(loud_a > 0.3, "first tone came back at RMS {loud_a:.4}");
    assert!(loud_b > 0.3, "second tone came back at RMS {loud_b:.4}");
    assert!(
        quiet < loud_a / 20.0,
        "the silent second came back at RMS {quiet:.4}, only {:.1}x below the tone; \
         the envelope was not preserved",
        loud_a / quiet.max(1e-9)
    );
}

#[test]
fn the_stream_is_a_conformant_ogg_opus_file() {
    let bytes = encode(&tone(440.0, 0.5, 0.4), RATE);

    // RFC 7845: capture pattern, then OpusHead alone on page 0, OpusTags
    // beginning page 1. A player that trusts the container will refuse
    // anything else.
    assert_eq!(&bytes[..4], b"OggS", "not an Ogg stream");
    assert_eq!(&bytes[28..36], b"OpusHead", "page 0 is not OpusHead");
    assert_eq!(bytes[36], 1, "OpusHead version must be 1");
    assert_eq!(bytes[37], 1, "the track must be mono (§9.5)");
    let head_rate = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
    assert_eq!(head_rate, RATE, "OpusHead must record the capture rate");
    // The BOS flag lives in byte 5 of the first page header.
    assert_eq!(bytes[5] & 0x02, 0x02, "page 0 must carry the BOS flag");

    let out = decode(&bytes);
    assert_eq!(out.channels, 1);
    assert!(
        out.pre_skip > 0,
        "OpusHead must declare the encoder lookahead, or every file plays \
         {}ms late",
        out.pre_skip
    );
    assert!(!out.truncated, "a cleanly finished stream is not truncated");
}

#[test]
fn the_bitrate_lands_where_9_5_says_it_does() {
    // §9.5's whole disk budget rests on 24 kbps. Continuous tone is close to
    // the worst case for VBR, so this is an upper bound with the codec given
    // no help at all.
    let secs = 10.0;
    let bytes = encode(&tone(440.0, secs, 0.4), RATE);
    let bps = bytes.len() as f64 * 8.0 / f64::from(secs as u32);
    assert!(
        bps < f64::from(BITRATE_BPS) * 1.25,
        "{bps:.0} bps against a {BITRATE_BPS} bps target — the budget in §9.5 \
         does not hold"
    );
    assert!(
        bps > f64::from(BITRATE_BPS) * 0.4,
        "{bps:.0} bps is suspiciously far under target; is anything being encoded?"
    );

    // Against the WAL's raw form — 16 kHz mono i16, which is 32 kB/s.
    let pcm_bytes = (RATE as f64 * secs as f64 * 2.0) as usize;
    let ratio = pcm_bytes as f64 / bytes.len() as f64;
    assert!(
        ratio > 8.0,
        "only {ratio:.1}x smaller than raw PCM; the point of encoding is the ratio"
    );
    println!("compression vs 16 kHz mono i16 PCM: {ratio:.2}x ({bps:.0} bps)");
}

#[test]
fn a_kill_mid_encode_leaves_a_playable_file_holding_all_but_the_last_page() {
    // CAP-10: `kill -9` at t=90 min must leave a *playable* file containing
    // >= 89 minutes. Simulated the only honest way — by never calling
    // `finish`, and by leaking the writer so that not even `Drop` runs. If
    // correctness depended on a destructor, a real SIGKILL would break it.
    let dir = tmpdir("kill");
    let path = dir.join("system.opus");

    let minutes = 90;
    let mut w = OpusOggWriter::create(&path, RATE).unwrap();
    // Push a second at a time so the encoder sees a realistic call pattern
    // and the page boundaries fall where they would in production.
    let second: Vec<i16> = tone(440.0, 1.0, 0.4)
        .iter()
        .map(|s| (s * 32_767.0) as i16)
        .collect();
    for _ in 0..minutes * 60 {
        w.push_i16(&second).unwrap();
    }
    std::mem::forget(w); // SIGKILL: no finish, no flush, no Drop.

    let out = decode_ogg_opus_file(&path, RATE).unwrap();
    let survived_ms = out.duration_ms();
    let wanted_ms = (minutes - 1) * 60 * 1_000;
    assert!(
        survived_ms >= wanted_ms,
        "a kill at {minutes} min left {survived_ms} ms; CAP-10 requires at \
         least {wanted_ms} ms"
    );
    // And the loss is bounded by exactly one page, not merely "under a minute".
    let lost_ms = minutes * 60 * 1_000 - survived_ms;
    assert!(
        lost_ms <= u64::from(PAGE_INTERVAL_MS + FRAME_MS),
        "lost {lost_ms} ms, more than the one page ({PAGE_INTERVAL_MS} ms) the \
         design promises"
    );

    // Playable, not merely present: the tail of what survived is still a
    // 440 Hz tone at the right level.
    let tail_start = out.samples.len().saturating_sub(RATE as usize);
    assert_tone(
        &out.samples[tail_start..],
        RATE,
        440.0,
        "the last second before the kill",
    );
}

#[test]
fn every_page_boundary_is_a_recovery_point() {
    // The property CAP-10 actually rests on: truncating the file at any byte
    // must still yield a playable prefix, because Ogg pages are independently
    // framed and CRC'd. Test it by chopping the file at several offsets.
    let dir = tmpdir("truncate");
    let path = dir.join("t.opus");
    let mut w = OpusOggWriter::with_page_interval(
        std::io::BufWriter::new(std::fs::File::create(&path).unwrap()),
        RATE,
        200,
    )
    .unwrap();
    w.push_f32(&tone(440.0, 5.0, 0.5)).unwrap();
    w.finish().unwrap();

    let whole = std::fs::read(&path).unwrap();
    let full = decode(&whole);
    assert!(full.duration_ms() >= 4_900);

    let mut previous = 0u64;
    for frac in [0.3f64, 0.5, 0.7, 0.9] {
        let cut = (whole.len() as f64 * frac) as usize;
        let out = decode_ogg_opus(Cursor::new(whole[..cut].to_vec()), RATE)
            .unwrap_or_else(|e| panic!("a file cut at {frac} of its length is unreadable: {e}"));
        let d = out.duration_ms();
        assert!(
            d > 0,
            "cutting at {frac} of the file yielded no audio at all"
        );
        assert!(
            d >= previous,
            "a longer prefix decoded to less audio ({d} ms after {previous} ms)"
        );
        assert!(
            d <= full.duration_ms(),
            "a truncated file decoded to more audio than the whole one"
        );
        // Playable, not merely parseable: what survives the cut is still the
        // tone that went in. A container that produced well-formed empty
        // pages would satisfy every duration assertion above.
        assert_tone(
            &out.samples[RATE as usize / 20..],
            RATE,
            440.0,
            &format!("the prefix left by a cut at {frac} of the file"),
        );
        previous = d;
    }
}

#[test]
fn arbitrary_push_sizes_re_block_into_20_ms_frames() {
    // The pump delivers whatever Core Audio felt like. If the writer only
    // worked on exact frame multiples, production would silently drop the
    // remainder of every callback.
    let pcm = tone(440.0, 2.0, 0.5);
    let dir = tmpdir("reblock");
    let path = dir.join("t.opus");
    let mut w = OpusOggWriter::create(&path, RATE).unwrap();

    // 37 samples is 2.3 ms — deliberately coprime with the 320-sample frame.
    for chunk in pcm.chunks(37) {
        w.push_f32(chunk).unwrap();
    }
    let stats = w.finish().unwrap();
    assert_eq!(
        stats.input_samples,
        pcm.len() as u64,
        "every pushed sample must be accounted for"
    );

    let out = decode_ogg_opus_file(&path, RATE).unwrap();
    let d = out.duration_ms();
    assert!(
        (1_950..=2_050).contains(&d),
        "2.0 s pushed 37 samples at a time came back as {d} ms"
    );
    assert_tone(
        &out.samples[800..],
        RATE,
        440.0,
        "audio pushed 37 samples at a time",
    );
}

#[test]
fn an_unsupported_rate_is_refused_rather_than_mangled() {
    // libopus takes 8/12/16/24/48 kHz. Accepting 44.1 and encoding it as if it
    // were 48 would produce a file that plays 9% slow — audible, and exactly
    // the kind of bug nobody traces back to the encoder.
    assert!(OpusOggWriter::new(Vec::new(), 44_100).is_err());
    assert!(OpusOggWriter::new(Vec::new(), 0).is_err());
    for rate in [8_000, 12_000, 16_000, 24_000, 48_000] {
        assert!(
            OpusOggWriter::new(Vec::new(), rate).is_ok(),
            "{rate} Hz must be accepted"
        );
    }
}

#[test]
fn silence_costs_far_less_than_speech_under_vbr() {
    // The 24 kbps figure in §9.5 is a VBR *target*, and the disk projection
    // only works out because real meetings are mostly not both people talking.
    // If this ever fails, VBR has been turned off and the budget doubles.
    let quiet = encode(&vec![0.0f32; RATE as usize * 5], RATE);
    let loud = encode(&tone(440.0, 5.0, 0.5), RATE);
    assert!(
        quiet.len() * 2 < loud.len(),
        "silence encoded to {} bytes against {} for tone; VBR is not engaged",
        quiet.len(),
        loud.len()
    );
}
