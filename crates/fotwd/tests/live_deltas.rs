//! Feeding the live transcript — issue #61.
//!
//! The hub, its 10 Hz flusher, the WebSocket and the UI renderer all exist
//! and sit idle: nothing in production ever calls `DeltaHub::publish`. The
//! producer is a tap on the session's collector — every `StreamEvent::Final`
//! already passes through it — handed in the same everything-is-an-argument
//! way as the taps, the transcription and the finisher.
//!
//! # The meeting-id decision, pinned here
//!
//! A live recording has no library id; persist mints one only at the end. The
//! deltas therefore carry the **session id** (`TranscriptSegment.session_id`,
//! which every segment already has), and nothing downstream minds: the UI's
//! renderer appends deltas without reading the id at all, and the post-stop
//! library refresh shows the persisted meeting under its real id. Threading a
//! pre-minted UUID through `persist_session` would touch the store for a
//! field no consumer reads today.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fotw_stt::Source;
use fotw_stt::transcript::{TimestampSource, TranscriptSegment};
use fotwd::serve::{delta_from, delta_partial};
use fotwd::session::{SegmentTap, TapKind};

fn seg(source: Source, start_ms: u64, text: &str) -> TranscriptSegment {
    TranscriptSegment {
        id: format!("{start_ms}-{text}"),
        session_id: "1787368804395-abcd1234".to_owned(),
        source,
        speaker: None,
        text: text.to_owned(),
        start_ms,
        end_ms: start_ms + 900,
        words: Vec::new(),
        confidence: None,
        language: None,
        is_final: true,
        revision: 0,
        provider: "deepgram".to_owned(),
        model: "nova-3".to_owned(),
        timestamp_source: TimestampSource::Provider,
    }
}

// ---------------------------------------------------------------- the tap

/// The default tap is silence, so every existing caller — the CLI, every
/// test — keeps exactly its old behavior without naming the feature.
#[test]
fn the_default_tap_swallows_segments_without_complaint() {
    SegmentTap::default().emit(&seg(Source::System, 0, "unheard"), TapKind::Final);
}

#[test]
fn a_tap_sees_every_segment_it_is_given() {
    let count = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&count);
    let tap = SegmentTap::new(move |_s, _k| {
        seen.fetch_add(1, Ordering::Relaxed);
    });

    tap.emit(&seg(Source::System, 0, "one"), TapKind::Final);
    tap.emit(&seg(Source::Mic, 500, "two"), TapKind::Partial);

    assert_eq!(count.load(Ordering::Relaxed), 2);
}

/// Clones share the closure — the recorder holds one end, the session task
/// the other, exactly like `StopSignal` and `SttErrors`.
#[test]
fn a_cloned_tap_reaches_the_same_closure() {
    let count = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&count);
    let tap = SegmentTap::new(move |_s, _k| {
        seen.fetch_add(1, Ordering::Relaxed);
    });

    let held = tap.clone();
    held.emit(&seg(Source::System, 0, "via the clone"), TapKind::Final);

    assert_eq!(count.load(Ordering::Relaxed), 1);
}

/// The tap receives the segment itself, not a summary of it — the publisher
/// needs the text, the times and the source.
#[test]
fn the_tap_is_handed_the_real_segment() {
    let heard = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = Arc::clone(&heard);
    let tap = SegmentTap::new(move |s, _k| {
        *sink.lock().unwrap() = s.text.clone();
    });

    tap.emit(&seg(Source::Mic, 100, "the exact words"), TapKind::Final);
    assert_eq!(*heard.lock().unwrap(), "the exact words");
}

// ------------------------------------------------------------- the mapping

#[test]
fn a_system_segment_becomes_a_system_delta() {
    let d = delta_from(7, &seg(Source::System, 4_200, "and that is why"));

    assert_eq!(d.meeting_id, "1787368804395-abcd1234");
    assert_eq!(d.idx, 7);
    assert_eq!(d.channel, "system");
    assert_eq!(d.text, "and that is why");
    assert_eq!(d.start_ms, 4_200);
    assert_eq!(d.end_ms, 5_100);
    assert!(d.is_final, "only finals reach the collector");
}

/// §7.5's "me vs them" survives the wire: the channel is what the UI styles
/// by, and losing it here would turn it back into a diarisation problem.
#[test]
fn a_mic_segment_becomes_a_mic_delta() {
    let d = delta_from(0, &seg(Source::Mic, 1_000, "wait, before you start"));
    assert_eq!(d.channel, "mic");
}

/// A revision, not a row: no index, not final — the renderer's replace
/// contract, pinned on the wire shape.
#[test]
fn a_partial_delta_has_no_index_and_is_not_final() {
    let d = delta_partial(&seg(Source::System, 2_000, "and that is wh"));
    assert_eq!(d.idx, -1);
    assert!(!d.is_final);
    assert_eq!(d.channel, "system");
    assert_eq!(d.text, "and that is wh");
}
