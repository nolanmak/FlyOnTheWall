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
/// provider configured is a supported, normal state, and the audio is still on
/// disk. Dropping it here would make the library quietly lossy in exactly the
/// case where the user most needs to find the recording.
pub fn persist_session(db: &mut Db, outcome: &SessionOutcome, title: &str) -> StoreResult<String> {
    // Validate before creating a row: a bad clock must not leave a phantom meeting.
    let capture_end = outcome
        .capture_ended_at_ms
        .map(|ended| {
            if ended < outcome.started_at_ms {
                return Err(fotw_store::StoreError::InvalidArgument(
                    "capture ends before it starts".into(),
                ));
            }
            i64::try_from(ended).map_err(|_| {
                fotw_store::StoreError::InvalidArgument(
                    "capture clock exceeds storage range".into(),
                )
            })
        })
        .transpose()?;
    // A source journal is authoritative. Its original identities/receipts must
    // bypass text dedupe and the legacy end-only capture retry path.
    if let Some(prepared) = speech_sources(outcome)? {
        let ended = capture_end.ok_or_else(|| {
            fotw_store::StoreError::InvalidArgument(
                "durable speech projection requires a frozen capture clock".into(),
            )
        })?;
        let session = outcome
            .dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                fotw_store::StoreError::InvalidArgument("capture has no session identity".into())
            })?;
        let mut meeting = NewMeeting::new(device_id(), timezone());
        meeting.title = title.into();
        meeting.started_at_ms = i64::try_from(outcome.started_at_ms).map_err(|_| {
            fotw_store::StoreError::InvalidArgument("capture clock exceeds storage range".into())
        })?;
        let id = db.finalize_speech_capture(session, meeting, ended, &prepared.sources)?;
        // The SQL commit is already durable. A failed acknowledgement can be
        // retried idempotently; it must never authorize deleting unsaved text.
        for checkpoint in prepared.checkpoints {
            let path = outcome.dir.join(match checkpoint.source {
                Source::System => "system.stt-projected.json",
                Source::Mic => "mic.stt-projected.json",
            });
            let projected = (|| {
                let source = match checkpoint.source {
                    Source::System => fotw_pipeline::replay::ReplaySource::System,
                    Source::Mic => fotw_pipeline::replay::ReplaySource::Mic,
                };
                let mut reader =
                    fotw_pipeline::replay::ReplayReader::open(&outcome.dir, session, source)?;
                if reader.is_complete()? && reader.saved_end()? == checkpoint.through_sample {
                    reader.mark_projected(checkpoint.through_sample, checkpoint.journal_bytes)?;
                }
                Ok::<_, std::io::Error>(())
            })();
            projected.map_err(|source| fotw_store::StoreError::Io {
                context: "acknowledging speech projection",
                path,
                source,
            })?;
        }
        return Ok(id);
    }
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
                crate::diag!(
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
    if outcome.started_at_ms > 0 || capture_end.is_some() {
        m.started_at_ms = outcome.started_at_ms as i64;
    }
    // A real capture has a stable WAL identity and frozen end. Commit its
    // meeting, transcript and note alias together so retry cannot split them.
    if let Some(ended) = capture_end {
        let session = outcome
            .dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                fotw_store::StoreError::InvalidArgument("capture has no session identity".into())
            })?;
        let rows: Vec<NewSegment> = outcome
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| to_row(i as i64, s))
            .collect();
        let transcript = outcome
            .segments
            .first()
            .map(|s| (s.provider.as_str(), s.model.as_str(), rows.as_slice()));
        return db.finalize_capture(session, m, ended, transcript);
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

    let ended = capture_end.unwrap_or_else(fotw_store::now_ms);
    db.meetings().finish(&meeting_id, ended)?;
    // `finish` stamps the timing only; the state machine is a separate
    // concern. Without this the meeting stays in `recording` forever and the
    // library shows every finished meeting as still running.
    db.meetings().set_state(&meeting_id, "ready")?;
    Ok(meeting_id)
}

/// Refresh a claimed session's source receipts before startup promotion can
/// retire its files. This also repairs a crash between SQL commit and the
/// filesystem acknowledgement, using the original saved capture clock.
pub fn refresh_speech_projection(
    db: &mut Db,
    dir: &std::path::Path,
    meeting_id: &str,
) -> StoreResult<()> {
    let has_journal = ["system.stt-coverage.jsonl", "mic.stt-coverage.jsonl"]
        .iter()
        .map(|name| dir.join(name).try_exists())
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|source| fotw_store::StoreError::Io {
            context: "inspecting pending speech",
            path: dir.to_owned(),
            source,
        })?
        .into_iter()
        .any(|exists| exists);
    if !has_journal {
        return Ok(());
    }
    let session = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            fotw_store::StoreError::InvalidArgument("capture has no session identity".into())
        })?;
    if db.session_note_meeting(session)?.as_deref() != Some(meeting_id) {
        return Err(fotw_store::StoreError::InvalidArgument(
            "pending speech has no matching saved capture identity".into(),
        ));
    }
    let (started, elapsed) = db.session_note_clock(session)?.ok_or_else(|| {
        fotw_store::StoreError::InvalidArgument("pending speech has no saved capture clock".into())
    })?;
    let ended = started
        .checked_add(elapsed)
        .ok_or_else(|| fotw_store::StoreError::InvalidArgument("capture clock overflow".into()))?;
    let title = db.meetings().get(meeting_id)?.title;
    let outcome = SessionOutcome {
        dir: dir.to_owned(),
        started_at_ms: u64::try_from(started).map_err(|_| {
            fotw_store::StoreError::InvalidArgument("invalid saved capture start".into())
        })?,
        capture_ended_at_ms: Some(u64::try_from(ended).map_err(|_| {
            fotw_store::StoreError::InvalidArgument("invalid saved capture end".into())
        })?),
        ..Default::default()
    };
    let projected = persist_session(db, &outcome, &title)?;
    if projected != meeting_id {
        return Err(fotw_store::StoreError::InvalidArgument(
            "speech projection changed capture identity".into(),
        ));
    }
    Ok(())
}

/// Index retained audio without a final clock, input seal or completion ack.
pub(crate) fn persist_interrupted(
    db: &mut Db,
    dir: &std::path::Path,
    started: u64,
    observed_elapsed: u64,
) -> StoreResult<String> {
    let outcome = SessionOutcome {
        dir: dir.to_owned(),
        ..Default::default()
    };
    let prepared = speech_sources(&outcome)?;
    let session = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| fotw_store::StoreError::InvalidArgument("capture has no identity".into()))?;
    let integer = |n| {
        i64::try_from(n).map_err(|_| {
            fotw_store::StoreError::InvalidArgument("capture clock exceeds storage range".into())
        })
    };
    let mut meeting = fotw_store::NewMeeting::new(device_id(), timezone());
    meeting.title = "Interrupted recording".into();
    meeting.started_at_ms = integer(started)?;
    let id = db.retain_interrupted_capture(
        session,
        meeting,
        integer(observed_elapsed)?,
        prepared.as_ref().map_or(&[], |p| p.sources.as_slice()),
    )?;
    if let Some(prepared) = prepared {
        for checkpoint in prepared.checkpoints {
            let source = match checkpoint.source {
                Source::System => fotw_pipeline::replay::ReplaySource::System,
                Source::Mic => fotw_pipeline::replay::ReplaySource::Mic,
            };
            let result = (|| {
                let mut reader = fotw_pipeline::replay::ReplayReader::open(dir, session, source)?;
                reader.mark_prefix_projected(checkpoint.through_sample, checkpoint.journal_bytes)
            })();
            result.map_err(|source| fotw_store::StoreError::Io {
                context: "acknowledging saved prefix projection",
                path: dir.to_owned(),
                source,
            })?;
        }
    }
    Ok(id)
}

struct ProjectionCheckpoint {
    source: Source,
    through_sample: u64,
    journal_bytes: u64,
}
struct PreparedProjection {
    sources: Vec<fotw_store::SpeechSource>,
    checkpoints: Vec<ProjectionCheckpoint>,
}

fn speech_sources(outcome: &SessionOutcome) -> StoreResult<Option<PreparedProjection>> {
    use crate::failover::coverage::CoverageJournal;
    use fotw_store::{SpeechCommit, SpeechEpoch, SpeechSegment, SpeechSource, StoreError};
    let session = outcome
        .dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let mut sources = Vec::new();
    let mut checkpoints = Vec::new();
    let integer = |value| {
        i64::try_from(value).map_err(|_| {
            StoreError::InvalidArgument("speech sample clock exceeds storage range".into())
        })
    };
    for (source, channel) in [(Source::System, "system"), (Source::Mic, "mic")] {
        let path = outcome.dir.join(format!("{channel}.stt-coverage.jsonl"));
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(StoreError::Io {
                    context: "inspecting speech journal",
                    path,
                    source,
                });
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(StoreError::InvalidArgument(
                    "speech journal is not an ordinary file".into(),
                ));
            }
            Ok(_) => {}
        }
        let snapshot =
            CoverageJournal::snapshot(&outcome.dir, session, source).map_err(|source| {
                StoreError::Io {
                    context: "reading speech journal",
                    path,
                    source,
                }
            })?;
        let through_sample = snapshot.commits.last().map_or_else(
            || snapshot.epochs[0].from_sample,
            |commit| commit.through_sample,
        );
        checkpoints.push(ProjectionCheckpoint {
            source,
            through_sample,
            journal_bytes: snapshot.valid_bytes,
        });
        let epochs = snapshot
            .epochs
            .iter()
            .map(|epoch| {
                Ok(SpeechEpoch {
                    number: integer(epoch.number)?,
                    from_sample: integer(epoch.from_sample)?,
                    provider: epoch.engine.provider.clone(),
                    model: epoch.engine.model.clone(),
                    revision: epoch.engine.revision.clone(),
                    capabilities: serde_json::to_value(&epoch.engine.capabilities).map_err(
                        |_| StoreError::InvalidArgument("invalid speech capabilities".into()),
                    )?,
                    reason: epoch.reason.map(|reason| {
                        serde_json::to_value(reason)
                            .expect("switch reason is serializable")
                            .as_str()
                            .expect("switch reason is a string")
                            .to_owned()
                    }),
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        let commits = snapshot
            .commits
            .iter()
            .map(|commit| {
                let segments = commit
                    .segments
                    .iter()
                    .enumerate()
                    .map(|(index, segment)| {
                        let mut segment = segment.clone();
                        let namespace = |label: &str| {
                            if source == Source::Mic && label == "me" {
                                "me".into()
                            } else {
                                format!("{channel}:e{}:{label}", commit.epoch)
                            }
                        };
                        segment.speaker = segment.speaker.as_deref().map(namespace);
                        for word in &mut segment.words {
                            word.speaker = word.speaker.as_deref().map(namespace);
                        }
                        Ok(SpeechSegment {
                            id: segment.id.clone(),
                            words: segment
                                .words
                                .iter()
                                .map(|word| {
                                    serde_json::to_value(word).map_err(|_| {
                                        StoreError::InvalidArgument(
                                            "invalid speech word record".into(),
                                        )
                                    })
                                })
                                .collect::<StoreResult<Vec<_>>>()?,
                            timestamp_source: match segment.timestamp_source {
                                fotw_stt::TimestampSource::Provider => "provider",
                                fotw_stt::TimestampSource::Estimated => "estimated",
                            }
                            .into(),
                            row: to_row(index as i64, &segment),
                        })
                    })
                    .collect::<StoreResult<Vec<_>>>()?;
                Ok(SpeechCommit {
                    epoch: integer(commit.epoch)?,
                    from_sample: integer(commit.from_sample)?,
                    through_sample: integer(commit.through_sample)?,
                    segments,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        sources.push(SpeechSource {
            source: channel.into(),
            epochs,
            commits,
        });
    }
    Ok((!sources.is_empty()).then_some(PreparedProjection {
        sources,
        checkpoints,
    }))
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
        capture_ended_at_ms: o.capture_ended_at_ms,
        system_samples: o.system_samples,
        mic_samples: o.mic_samples,
        stt_errors: o.stt_errors.clone(),
        system_buffers: o.system_buffers,
        mic_buffers: o.mic_buffers,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::LegBuffers;
    use fotw_store::DbKey;

    /// The premise the `meeting_ready` announcement rests on (#78).
    ///
    /// The daemon announces `persisted` the instant [`persist_session`]
    /// returns an id — before promotion, because the Opus encode can take
    /// minutes and the meeting is already in the library. That is only
    /// correct if the row is queryable *at that point*, in the state the list
    /// view expects. It is `set_state` at the tail here that makes it so, and
    /// `finish` alone would not: a row stuck in `recording` lists as a meeting
    /// still in progress, which is what an announced-but-unfinished meeting
    /// would look like in the UI.
    ///
    /// A `Db::open` over a temp directory rather than `open_library`: that
    /// reads the OS keychain, and a test that touches the user's keychain
    /// raises an approval dialog on every rebuild.
    #[test]
    fn a_persisted_session_is_immediately_listable_as_ready() {
        let dir = std::env::temp_dir().join(format!(
            "fotwd-persist-unit-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(dir.join("db.sqlite3"), &DbKey::from_bytes([0x78; 32])).unwrap();

        let outcome = SessionOutcome {
            dir: dir.clone(),
            started_at_ms: 1_754_900_000_000,
            capture_ended_at_ms: None,
            system_samples: 480_000,
            mic_samples: 0,
            stt_errors: Vec::new(),
            system_buffers: LegBuffers {
                silent: 0,
                total: 100,
            },
            mic_buffers: None,
            dropped_samples: 0,
            segments: Vec::new(),
        };
        let id = persist_session(&mut db, &outcome, "Untitled recording — 1754900000").unwrap();

        let rows = db.meetings().list(50, 0).unwrap();
        let found = rows
            .iter()
            .find(|m| m.id == id)
            .expect("the id the daemon announces must already be in the list");
        assert_eq!(
            found.state, "ready",
            "announcing a meeting that still lists as `recording` would put a \
             finished meeting on the dashboard as one in progress"
        );
        assert_eq!(found.started_at_ms, 1_754_900_000_000);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
