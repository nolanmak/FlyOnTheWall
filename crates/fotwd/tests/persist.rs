//! A finished session becomes a queryable meeting.
//!
//! Until this exists a transcript lives only in `stt.jsonl` beside the audio,
//! which is fine for recovery and useless for search, summarisation or the UI.
//! This is the step that makes the recording a *library entry*.

use fotw_store::{Db, DbKey};
use fotw_stt::{Source, TimestampSource, TranscriptSegment, Word};
use fotwd::persist;
use fotwd::session::{LegBuffers, SessionOutcome};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fotwd-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn db_at(dir: &std::path::Path) -> Db {
    // A fixed test key: this exercises the real encrypted path without
    // needing a keychain, which CI does not have.
    let key = DbKey::from_bytes([0xab; 32]);
    Db::open(dir.join("db.sqlite3"), &key).unwrap()
}

fn seg(
    idx: u64,
    text: &str,
    start: u64,
    source: Source,
    speaker: Option<&str>,
) -> TranscriptSegment {
    TranscriptSegment {
        id: format!("seg-{idx}"),
        session_id: "test-session".into(),
        source,
        speaker: speaker.map(str::to_string),
        text: text.into(),
        start_ms: start,
        end_ms: start + 1_000,
        words: vec![Word {
            text: text.split_whitespace().next().unwrap_or("x").into(),
            start_ms: start,
            end_ms: start + 200,
            confidence: Some(0.94),
            speaker: speaker.map(str::to_string),
        }],
        confidence: Some(0.93),
        language: Some("en".into()),
        is_final: true,
        revision: 0,
        provider: "deepgram".into(),
        model: "nova-3".into(),
        timestamp_source: TimestampSource::Provider,
    }
}

fn outcome(dir: &std::path::Path, segments: Vec<TranscriptSegment>) -> SessionOutcome {
    SessionOutcome {
        dir: dir.to_path_buf(),
        started_at_ms: 0,
        system_samples: 480_000,
        mic_samples: 240_000,
        system_buffers: LegBuffers {
            silent: 2,
            total: 400,
        },
        mic_buffers: Some(LegBuffers {
            silent: 0,
            total: 400,
        }),
        dropped_samples: 0,
        segments,
        stt_errors: Vec::new(),
    }
}

#[test]
fn a_session_becomes_a_queryable_meeting_with_its_transcript() {
    let dir = tmpdir("basic");
    let mut db = db_at(&dir);

    let segments = vec![
        seg(
            0,
            "The quarterly numbers came in above target.",
            0,
            Source::System,
            Some("S0"),
        ),
        seg(
            1,
            "Priya will follow up with infra by Friday.",
            1_000,
            Source::System,
            Some("S1"),
        ),
        seg(2, "Sounds good to me.", 2_000, Source::Mic, Some("me")),
    ];
    let id = persist::persist_session(&mut db, &outcome(&dir, segments), "Weekly sync").unwrap();

    let m = db.meetings().get(&id).unwrap();
    assert_eq!(m.title, "Weekly sync");
    assert_eq!(
        m.state, "ready",
        "a persisted session is finished, not still recording"
    );
    assert!(m.ended_at_ms.is_some(), "ended_at must be stamped");

    // The transcript is queryable in order.
    let tid = persist::primary_transcript_id(&mut db, &id).unwrap();
    let text = db.meetings().transcript_text(&tid).unwrap();
    assert_eq!(text.len(), 3);
    assert!(text[0].1.contains("quarterly"));
    assert!(text[2].1.contains("Sounds good"));
    assert!(
        text[0].0 <= text[1].0 && text[1].0 <= text[2].0,
        "segments must come back in time order"
    );
}

/// The two legs must stay distinguishable all the way into storage. Losing
/// the channel here would turn "me vs them" — which capture gets for free by
/// running two devices — back into a diarisation problem.
#[test]
fn the_channel_survives_into_the_database() {
    let dir = tmpdir("channel");
    let mut db = db_at(&dir);

    let segments = vec![
        seg(0, "Them speaking.", 0, Source::System, Some("S0")),
        seg(1, "Me speaking.", 1_000, Source::Mic, Some("me")),
    ];
    let id = persist::persist_session(&mut db, &outcome(&dir, segments), "Two legs").unwrap();
    let tid = persist::primary_transcript_id(&mut db, &id).unwrap();

    let channels: Vec<String> = db
        .conn()
        .prepare("SELECT channel FROM segments WHERE transcript_id = ?1 ORDER BY idx")
        .unwrap()
        .query_map([&tid], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(channels, vec!["system", "mic"]);
}

#[test]
fn a_session_with_no_transcript_still_becomes_a_meeting() {
    // Recording without a provider configured is a supported, normal state —
    // the audio is on disk and can be transcribed later. It must still show
    // up in the library rather than vanishing.
    let dir = tmpdir("notranscript");
    let mut db = db_at(&dir);

    let id = persist::persist_session(&mut db, &outcome(&dir, Vec::new()), "Silent one").unwrap();
    let m = db.meetings().get(&id).unwrap();
    assert_eq!(m.title, "Silent one");
    assert!(
        persist::primary_transcript_id(&mut db, &id).is_none(),
        "no segments means no transcript row, not an empty one"
    );
}

#[test]
fn word_level_timings_round_trip_through_the_blob() {
    let dir = tmpdir("words");
    let mut db = db_at(&dir);

    let segments = vec![seg(0, "hello world", 0, Source::System, Some("S0"))];
    let id = persist::persist_session(&mut db, &outcome(&dir, segments), "Words").unwrap();
    let tid = persist::primary_transcript_id(&mut db, &id).unwrap();

    let blob: Option<Vec<u8>> = db
        .conn()
        .query_row(
            "SELECT words FROM segments WHERE transcript_id = ?1 AND idx = 0",
            [&tid],
            |r| r.get(0),
        )
        .unwrap();
    let blob = blob.expect("word timings must be stored");
    assert!(!blob.is_empty());

    let decoded = persist::decode_words(&blob).unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].text, "hello");
    assert_eq!(decoded[0].confidence, Some(0.94));
}

#[test]
fn persisting_twice_creates_two_meetings_not_a_duplicate_transcript() {
    // Each recording is its own meeting. Re-running the same session object
    // must not silently merge into the previous one, which would make the
    // library quietly lossy.
    let dir = tmpdir("twice");
    let mut db = db_at(&dir);
    let o = outcome(&dir, vec![seg(0, "one", 0, Source::System, None)]);

    let a = persist::persist_session(&mut db, &o, "First").unwrap();
    let b = persist::persist_session(&mut db, &o, "Second").unwrap();
    assert_ne!(a, b);
    assert_eq!(db.meetings().list(10, 0).unwrap().len(), 2);
}

/// A meeting's duration comes from when capture began, not from when it was
/// written to the library. Those differ by the whole length of the meeting,
/// and stamping the later one makes every recording read as zero seconds —
/// which is exactly what the first version of this did.
#[test]
fn the_duration_reflects_the_recording_not_the_write() {
    let dir = tmpdir("duration");
    let mut db = db_at(&dir);

    let mut o = outcome(&dir, vec![seg(0, "hello", 0, Source::System, None)]);
    o.started_at_ms = (fotw_store::now_ms() as u64).saturating_sub(90_000);

    let id = persist::persist_session(&mut db, &o, "Ninety seconds").unwrap();
    let m = db.meetings().get(&id).unwrap();
    let secs = m.duration_ms.unwrap_or(0) / 1000;
    assert!(
        (85..=95).contains(&secs),
        "expected roughly 90s, got {secs}s"
    );
}
