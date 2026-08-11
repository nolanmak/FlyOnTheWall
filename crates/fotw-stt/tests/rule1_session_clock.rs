//! Spec 7.2, normalization rule 1.
//!
//! > `startMs`/`endMs` are **always** milliseconds from session t0 on our own
//! > clock, never provider-relative. Adapters add the connection's t0 offset;
//! > on reconnect the offset is recomputed so timestamps stay continuous.
//!
//! The reconnect half of that sentence is the load-bearing half. A provider
//! socket that dies at minute 40 comes back with its own clock at zero, and
//! STT-09 replays the last 30 s of PCM into it. Without recomputing the offset
//! every timestamp after the reconnect lands back at the start of the meeting,
//! which silently corrupts every seek, every citation and every export.

use fotw_stt::{SessionClock, seconds_to_ms};

#[test]
fn a_fresh_connection_at_session_start_is_the_identity() {
    let clock = SessionClock::new();
    assert_eq!(clock.offset_ms(), 0);
    assert_eq!(clock.to_session_ms(0), 0);
    assert_eq!(clock.to_session_ms(1_500), 1_500);
}

#[test]
fn a_connection_opened_mid_session_adds_its_t0_offset() {
    // The provider stream opened 12 s into the meeting, so its own zero is our
    // 12_000.
    let clock = SessionClock::opened_at(12_000);

    assert_eq!(clock.offset_ms(), 12_000);
    assert_eq!(clock.to_session_ms(0), 12_000);
    assert_eq!(clock.to_session_ms(340), 12_340);
}

#[test]
fn reconnecting_recomputes_the_offset_so_timestamps_stay_continuous() {
    // A 40-minute meeting. The socket dies at 40:00 and STT-09 replays the last
    // 30 s of PCM into the new connection, so the new connection's provider
    // clock zero corresponds to session time 39:30.
    let mut clock = SessionClock::new();

    // Provider-relative times before the drop, in seconds as Deepgram reports.
    let before: Vec<u64> = [2_399.0_f64, 2_399.6, 2_400.0]
        .into_iter()
        .map(|s| clock.to_session_ms(seconds_to_ms(s)))
        .collect();
    assert_eq!(before, vec![2_399_000, 2_399_600, 2_400_000]);

    let replay_from_ms = 2_370_000; // 39:30
    clock.reconnected_at(replay_from_ms);
    assert_eq!(clock.offset_ms(), replay_from_ms);

    // The new connection's clock restarts at zero. These provider times are
    // *smaller* than the ones above; only the recomputed offset keeps the
    // session timeline moving forward.
    let after: Vec<u64> = [30.4_f64, 31.0, 92.5]
        .into_iter()
        .map(|s| clock.to_session_ms(seconds_to_ms(s)))
        .collect();
    assert_eq!(after, vec![2_400_400, 2_401_000, 2_462_500]);

    // No discontinuity: the timeline never jumps backwards across the reconnect.
    let timeline: Vec<u64> = before.iter().chain(after.iter()).copied().collect();
    assert!(
        timeline.windows(2).all(|w| w[1] >= w[0]),
        "timestamps went backwards across the reconnect: {timeline:?}"
    );

    // And it never jumps *forwards* into a gap either — the first segment after
    // the reconnect picks up within a frame of where the last one ended.
    let gap = after[0] - before[2];
    assert!(gap < 1_000, "unexplained {gap} ms hole at the reconnect");
}

#[test]
fn without_recomputing_the_offset_the_timeline_would_collapse() {
    // The control case for the test above: this is the bug rule 1 exists to
    // prevent. Same provider times, offset left at its original value.
    let clock = SessionClock::new();

    let last_before = clock.to_session_ms(seconds_to_ms(2_400.0));
    let first_after = clock.to_session_ms(seconds_to_ms(30.4));

    assert!(
        first_after < last_before,
        "the control case must show the backwards jump the reconnect logic fixes"
    );
}

#[test]
fn offsets_accumulate_across_repeated_reconnects() {
    // ElevenLabs' `session_time_limit_exceeded` means long meetings reconnect
    // several times; the offsets must not compound or drift.
    let mut clock = SessionClock::new();
    let mut timeline = vec![clock.to_session_ms(0)];

    for hop in 1..=5_u64 {
        let resume_at = hop * 600_000; // every 10 minutes
        clock.reconnected_at(resume_at);
        timeline.push(clock.to_session_ms(0));
        timeline.push(clock.to_session_ms(1_234));
    }

    assert!(timeline.windows(2).all(|w| w[1] >= w[0]), "{timeline:?}");
    assert_eq!(*timeline.last().unwrap(), 3_000_000 + 1_234);
}

#[test]
fn seconds_convert_to_milliseconds_by_rounding() {
    // Deepgram reports `start`/`end` in SECONDS as floats. Everything above this
    // crate is integer milliseconds, so this conversion is a normalization step
    // in its own right.
    assert_eq!(seconds_to_ms(0.0), 0);
    assert_eq!(seconds_to_ms(10.52), 10_520);
    assert_eq!(seconds_to_ms(1.0), 1_000);
    assert_eq!(seconds_to_ms(3_600.0), 3_600_000);

    // Round to nearest, not truncate: truncation biases every timestamp early
    // and the error accumulates across a 2-hour meeting's worth of words.
    assert_eq!(seconds_to_ms(1.2345), 1_235);
    assert_eq!(seconds_to_ms(1.2344), 1_234);
    assert_eq!(seconds_to_ms(0.0004), 0);
    assert_eq!(seconds_to_ms(0.0006), 1);

    // Binary floats cannot represent 8.7 exactly; the rounded result must still
    // be the obvious one rather than 8699.
    assert_eq!(seconds_to_ms(8.7), 8_700);
    assert_eq!(seconds_to_ms(0.1 + 0.2), 300);

    // A hostile or broken peer must not produce a nonsense timestamp.
    assert_eq!(seconds_to_ms(-1.0), 0);
    assert_eq!(seconds_to_ms(f64::NAN), 0);
    assert_eq!(seconds_to_ms(f64::INFINITY), u64::MAX);
    assert_eq!(seconds_to_ms(f64::NEG_INFINITY), 0);
}
