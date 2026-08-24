//! Turning a finished session into a library entry.
//!
//! Until this runs, a transcript lives only in `stt.jsonl` beside the audio.
//! That is exactly right for crash recovery and useless for search,
//! summarisation, or anything the UI wants to show. This module is the step
//! that makes a recording a *meeting*.
//!
//! # Ordering
//!
//! Persistence happens **after** the WAL is finalized, never instead of it.
//! The database is an index over the session directory (§9.1); if this fails,
//! the meeting is still on disk and can be imported again. Reversing that —
//! writing to SQLite first and treating the files as a cache — would make a
//! database error a lost meeting.

use fotw_store::{Db, NewMeeting, NewSegment, Result as StoreResult};
use fotw_stt::{Source, TranscriptSegment, Word};

use crate::session::SessionOutcome;

/// Write a finished session into the library, returning the meeting id.
///
/// A session with no segments still becomes a meeting: recording without a
/// provider configured is a supported, normal state, and the audio can be
/// transcribed later. Dropping it here would make the library quietly lossy
/// in exactly the case where the user most needs to find the recording.
pub fn persist_session(db: &mut Db, outcome: &SessionOutcome, title: &str) -> StoreResult<String> {
    // Cross-leg dedupe happens HERE, not at session end: `stt.jsonl` is the
    // crash-recovery record and stays raw, so a matcher misfire is always
    // recoverable from the session directory. The database — and everything
    // that reads it: export, titles, the detail API — gets the deduped view.
    let deduped;
    let outcome =
        if crate::session::text_dedupe_enabled(std::env::var("FOTW_TEXT_DEDUPE").ok().as_deref()) {
            let mut segments = outcome.segments.clone();
            let dropped = crate::session::dedupe_cross_leg(&mut segments);
            if dropped > 0 {
                eprintln!(
                    "  dedupe     : dropped {dropped} mic segment(s) that duplicated the \
                 system leg (speaker echo)"
                );
            }
            deduped = SessionOutcome {
                segments,
                ..outcome_shallow(outcome)
            };
            &deduped
        } else {
            outcome
        };
    let mut m = NewMeeting::new(device_id(), timezone());
    m.title = title.to_owned();
    // The real capture start, not now: they differ by the length of the
    // meeting, and using now would make every recording show zero duration.
    if outcome.started_at_ms > 0 {
        m.started_at_ms = outcome.started_at_ms as i64;
    }
    let meeting_id = db.meetings().create(m)?;

    if !outcome.segments.is_empty() {
        // Provider and model come from the segments themselves rather than
        // from configuration: after a mid-meeting failover they are the only
        // record of who actually produced this text.
        let provider = outcome.segments[0].provider.clone();
        let model = outcome.segments[0].model.clone();
        let transcript_id =
            db.meetings()
                .create_transcript(&meeting_id, &provider, &model, true)?;

        let rows: Vec<NewSegment> = outcome
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| to_row(i as i64, s))
            .collect();
        db.meetings().append_segments(&transcript_id, &rows)?;
    }

    let ended = fotw_store::now_ms();
    db.meetings().finish(&meeting_id, ended)?;
    // `finish` stamps the timing only; the state machine is a separate
    // concern. Without this the meeting stays in `recording` forever and the
    // library shows every finished meeting as still running.
    db.meetings().set_state(&meeting_id, "ready")?;
    Ok(meeting_id)
}

/// The primary transcript for a meeting, if it has one.
///
/// Delegates to the store rather than repeating the query: the hand-rolled
/// version used `.ok()`, which turned a real SQLite failure into "no
/// transcript" and would have shown an empty meeting instead of an error.
pub fn primary_transcript_id(db: &mut Db, meeting_id: &str) -> Option<String> {
    db.meetings()
        .primary_transcript_id(meeting_id)
        .ok()
        .flatten()
}

/// What one retroactive dedupe pass did.
#[derive(Debug)]
pub struct DedupeReport {
    /// Segments before the pass.
    pub before: usize,
    /// Mic segments dropped as speaker echo.
    pub dropped: usize,
    /// The new primary transcript id, when one was written.
    pub new_transcript: Option<String>,
}

/// Re-run the cross-leg dedupe over a stored meeting (#71 follow-through).
///
/// The library already holds meetings persisted before this pass existed —
/// their transcripts carry every speaker-echo duplicate. This rewrites them
/// the way the store already versions derived artifacts: a NEW transcript
/// becomes primary and the old one is retained untouched, so
/// `set_primary_transcript(&old)` is a full revert. Idempotent: a clean
/// transcript writes nothing.
///
/// # Errors
///
/// Whatever the store failed with.
pub fn dedupe_meeting(db: &mut Db, meeting_id: &str) -> StoreResult<DedupeReport> {
    let Some(old_transcript) = primary_transcript_id(db, meeting_id) else {
        return Ok(DedupeReport {
            before: 0,
            dropped: 0,
            new_transcript: None,
        });
    };
    let mut segments = crate::summarize::load_segments(db, &old_transcript)?;
    crate::session::order_segments(&mut segments);
    let before = segments.len();
    let dropped = crate::session::dedupe_cross_leg(&mut segments);
    if dropped == 0 {
        return Ok(DedupeReport {
            before,
            dropped: 0,
            new_transcript: None,
        });
    }

    // Provider and model belong to the transcript row, not the segments;
    // the export view is the one existing reader that surfaces them.
    let (provider, model) = db
        .export_meeting(meeting_id)?
        .transcripts
        .iter()
        .find(|t| t.id == old_transcript)
        .map(|t| (t.provider.clone(), t.model.clone()))
        .unwrap_or_else(|| ("deepgram".to_owned(), "unknown".to_owned()));

    // `make_primary: true` demotes the old primary in the same transaction;
    // the old rows stay for revert. Dense re-index, as persist writes.
    let new_transcript = db
        .meetings()
        .create_transcript(meeting_id, &provider, &model, true)?;
    let rows: Vec<NewSegment> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| to_row(i as i64, s))
        .collect();
    db.meetings().append_segments(&new_transcript, &rows)?;

    Ok(DedupeReport {
        before,
        dropped,
        new_transcript: Some(new_transcript),
    })
}

/// A field-for-field copy of the outcome minus its segments, so the deduped
/// view above can borrow the same shape without mutating the caller's.
fn outcome_shallow(o: &SessionOutcome) -> SessionOutcome {
    SessionOutcome {
        dir: o.dir.clone(),
        started_at_ms: o.started_at_ms,
        system_samples: o.system_samples,
        mic_samples: o.mic_samples,
        stt_errors: o.stt_errors.clone(),
        silent_buffers: o.silent_buffers,
        total_buffers: o.total_buffers,
        dropped_samples: o.dropped_samples,
        segments: Vec::new(),
    }
}

fn to_row(idx: i64, s: &TranscriptSegment) -> NewSegment {
    NewSegment {
        idx,
        start_ms: s.start_ms as i64,
        end_ms: s.end_ms as i64,
        // The channel is the whole reason "me vs them" is free: capture runs
        // two physically separate devices, so losing this on the way into
        // storage would turn it back into a diarisation problem.
        channel: match s.source {
            Source::Mic => "mic",
            Source::System => "system",
        }
        .to_owned(),
        speaker_label: s.speaker.clone(),
        person_id: None,
        text: s.text.clone(),
        confidence: s.confidence,
        is_final: s.is_final,
        words: encode_words(&s.words),
    }
}

/// Word timings as a compressed JSON blob.
///
/// A BLOB rather than rows because a heavy user generates roughly 8.4M words
/// a year and no query ever wants a single word — they are read only when a
/// segment is played back or highlighted (§9.3).
#[must_use]
pub fn encode_words(words: &[Word]) -> Option<Vec<u8>> {
    if words.is_empty() {
        return None;
    }
    serde_json::to_vec(words).ok()
}

/// Read word timings back out of a stored blob.
pub fn decode_words(blob: &[u8]) -> Option<Vec<Word>> {
    serde_json::from_slice(blob).ok()
}

/// A stable per-machine identifier for the merge triple.
///
/// Not a hardware serial or anything else identifying: this only has to be
/// stable on one machine and distinct between two, and a random id persisted
/// alongside the library satisfies both without collecting anything.
fn device_id() -> String {
    "local".to_owned()
}

fn timezone() -> String {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy().to_string();
            s.split_once("zoneinfo/").map(|(_, tz)| tz.to_owned())
        })
        .unwrap_or_else(|| "UTC".to_owned())
}
