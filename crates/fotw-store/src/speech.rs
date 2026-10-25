//! Atomic, idempotent projection of durable source journals into the library.
use std::collections::HashSet;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{Db, NewMeeting, NewSegment, Result, StoreError};

/// Provider provenance at one source's committed-audio boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechEpoch {
    /// Monotonic source-local fencing token, starting at zero.
    pub number: i64,
    /// Absolute sample on the 16 kHz session clock.
    pub from_sample: i64,
    /// Actual speech provider, never the configured preference.
    pub provider: String,
    /// Actual model identifier.
    pub model: String,
    /// Model checksum or installed revision, when available.
    pub revision: Option<String>,
    /// Capabilities active for this epoch.
    pub capabilities: serde_json::Value,
    /// Switch reason; absent for the initial engine.
    pub reason: Option<String>,
}

/// Metadata for one actual engine on the primary transcript's recording clock.
/// Confirmed intervals do not imply complete or accurate recognition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechProvenance {
    /// Original capture source (`system` or `mic`).
    pub source: String,
    /// Source-local provider epoch.
    pub epoch: i64,
    /// Engine boundary in original 16 kHz samples.
    pub from_sample: i64,
    /// Last confirmed receipt boundary, absent if this engine has no receipts.
    pub through_sample: Option<i64>,
    /// Actual provider.
    pub provider: String,
    /// Actual model.
    pub model: String,
    /// Recorded revision, when known.
    pub revision: Option<String>,
    /// Recorded capabilities, never inferred from current settings.
    pub capabilities: serde_json::Value,
    /// Recorded provider switch reason.
    pub reason: Option<String>,
}

impl Db {
    /// Read primary-transcript engine history without loading inference payloads.
    /// Older transcripts without recorded provenance return an empty history.
    pub fn speech_provenance(&mut self, meeting_id: &str) -> Result<Vec<SpeechProvenance>> {
        let Some(transcript) = self.meetings().primary_transcript_id(meeting_id)? else {
            return Ok(Vec::new());
        };
        let mut statement = self.conn().prepare(
            "SELECT e.source,e.epoch,e.from_sample,MAX(r.through_sample),e.provider,e.model,e.revision,e.capabilities_json,e.reason
             FROM speech_epochs e LEFT JOIN speech_receipts r ON r.epoch_id=e.id
             WHERE e.meeting_id=?1 AND e.transcript_id=?2
             GROUP BY e.id ORDER BY e.source,e.epoch",
        )?;
        let rows = statement.query_map(params![meeting_id, transcript], |row| {
            let capabilities: String = row.get(7)?;
            Ok(SpeechProvenance {
                source: row.get(0)?,
                epoch: row.get(1)?,
                from_sample: row.get(2)?,
                through_sample: row.get(3)?,
                provider: row.get(4)?,
                model: row.get(5)?,
                revision: row.get(6)?,
                capabilities: serde_json::from_str(&capabilities).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                reason: row.get(8)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/// One settled segment with its original durable identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechSegment {
    /// Stable original STT identity; retries must reuse it.
    pub id: String,
    /// Whether original timestamps were provider-reported or estimated.
    pub timestamp_source: String,
    /// Original uncompressed word records. Compressed projection bytes are not
    /// receipt identity: compression output may change across runtime upgrades.
    pub words: Vec<serde_json::Value>,
    /// Original inference payload, independent of later user corrections.
    /// `idx` is receipt-local; the projected timeline is ordered by time.
    pub row: NewSegment,
}

/// An immutable receipt for a contiguous completed interval, including silence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechCommit {
    /// The provider epoch that produced this interval.
    pub epoch: i64,
    /// Inclusive absolute 16 kHz sample.
    pub from_sample: i64,
    /// Exclusive absolute 16 kHz sample.
    pub through_sample: i64,
    /// Final segments; empty means acknowledged silence.
    pub segments: Vec<SpeechSegment>,
}

/// A validated full snapshot of one source's journal, possibly still incomplete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechSource {
    /// `system` or `mic`; identities are independent even for identical words.
    pub source: String,
    /// Every provider epoch in journal order.
    pub epochs: Vec<SpeechEpoch>,
    /// Every committed interval in journal order.
    pub commits: Vec<SpeechCommit>,
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidArgument(message.into())
}
fn epoch_id(session: &str, source: &str, number: i64) -> String {
    format!("{session}:{source}:epoch:{number}")
}
fn receipt_id(session: &str, source: &str, from: i64) -> String {
    format!("{session}:{source}:sample:{from}")
}

fn receipt_payload(commit: &SpeechCommit) -> Result<String> {
    let mut payload =
        serde_json::to_value(commit).map_err(|_| invalid("invalid speech receipt"))?;
    for segment in payload["segments"]
        .as_array_mut()
        .expect("typed segments are an array")
    {
        segment["row"]
            .as_object_mut()
            .expect("typed segment row is an object")
            .remove("words");
    }
    serde_json::to_string(&payload).map_err(|_| invalid("invalid speech receipt"))
}

impl Db {
    /// Project source journals and the capture/notes handoff in one transaction.
    /// Exact receipts are no-ops, preserving edited segments and notes. New
    /// intervals append to the original transcript without changing a user's
    /// primary-transcript selection. Conflicts roll the entire projection back.
    pub fn finalize_speech_capture(
        &mut self,
        session: &str,
        meeting: NewMeeting,
        ended_at_ms: i64,
        sources: &[SpeechSource],
    ) -> Result<String> {
        self.project_capture(session, meeting, Some(ended_at_ms), 0, sources)
    }

    /// Index an interrupted recording and any validated committed receipts,
    /// retaining an unknown end and preserving notes/corrections on retry.
    pub fn retain_interrupted_capture(
        &mut self,
        session: &str,
        meeting: NewMeeting,
        observed_elapsed_ms: i64,
        sources: &[SpeechSource],
    ) -> Result<String> {
        self.project_capture(session, meeting, None, observed_elapsed_ms, sources)
    }

    fn project_capture(
        &mut self,
        session: &str,
        meeting: NewMeeting,
        ended_at_ms: Option<i64>,
        observed_elapsed: i64,
        sources: &[SpeechSource],
    ) -> Result<String> {
        if !sources.is_empty() || ended_at_ms.is_some() {
            validate(sources)?;
        }
        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let meeting_id = if let Some(ended) = ended_at_ms {
            crate::session_notes::finalize_in_transaction(&tx, session, meeting, ended, None)?
        } else {
            crate::session_notes::retain_in_transaction(&tx, session, meeting, observed_elapsed)?
        };
        if sources.is_empty() {
            tx.commit()?;
            return Ok(meeting_id);
        }
        let transcript: Option<String> = tx
            .query_row(
                "SELECT transcript_id FROM speech_epochs WHERE meeting_id=?1 LIMIT 1",
                [&meeting_id],
                |r| r.get(0),
            )
            .optional()?;
        let transcript = match transcript {
            Some(id) => id,
            None => {
                let has_primary: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM transcripts WHERE meeting_id=?1 AND is_primary=1)",
                    [&meeting_id],
                    |row| row.get(0),
                )?;
                crate::repo::insert_transcript(
                    &tx,
                    &meeting_id,
                    "mixed",
                    "source-journal",
                    !has_primary,
                )?
            }
        };
        let mut changed_segments = false;
        for source in sources {
            for epoch in &source.epochs {
                let id = epoch_id(session, &source.source, epoch.number);
                let previous = tx.query_row(
                    "SELECT epoch,from_sample,provider,model,revision,capabilities_json,reason FROM speech_epochs WHERE id=?1 AND meeting_id=?2 AND transcript_id=?3",
                    params![id, meeting_id, transcript], |r| Ok(SpeechEpoch {
                        number:r.get(0)?, from_sample:r.get(1)?, provider:r.get(2)?, model:r.get(3)?, revision:r.get(4)?,
                        capabilities: r.get::<_,serde_json::Value>(5)?, reason:r.get(6)?,
                    }),
                ).optional()?;
                if let Some(previous) = previous {
                    if previous != *epoch {
                        return Err(invalid("conflicting persisted provider epoch"));
                    }
                    continue;
                }
                tx.execute("INSERT INTO speech_epochs(id,meeting_id,transcript_id,session_id,source,epoch,from_sample,provider,model,revision,capabilities_json,reason) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![id,meeting_id,transcript,session,source.source,epoch.number,epoch.from_sample,epoch.provider,epoch.model,epoch.revision,serde_json::to_string(&epoch.capabilities).map_err(|_|invalid("invalid capabilities"))?,epoch.reason])?;
            }
            let mut cursor: i64 = tx.query_row(
                "SELECT COALESCE(MAX(r.through_sample),?3) FROM speech_receipts r JOIN speech_epochs e ON e.id=r.epoch_id WHERE e.session_id=?1 AND e.source=?2",
                params![session,source.source,source.epochs[0].from_sample], |r|r.get(0),
            )?;
            for commit in &source.commits {
                let id = receipt_id(session, &source.source, commit.from_sample);
                let payload = receipt_payload(commit)?;
                let previous: Option<String> = tx.query_row(
                    "SELECT payload_json FROM speech_receipts WHERE id=?1 AND meeting_id=?2 AND transcript_id=?3",
                    params![id,meeting_id,transcript], |r|r.get(0),
                ).optional()?;
                if let Some(previous) = previous {
                    if previous != payload {
                        return Err(invalid("conflicting persisted speech receipt"));
                    }
                    continue;
                }
                if commit.from_sample != cursor {
                    return Err(invalid("persisted speech coverage has a gap or overlap"));
                }
                let first_idx: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(idx)+1,0) FROM segments WHERE transcript_id=?1",
                    [&transcript],
                    |r| r.get(0),
                )?;
                for (position, segment) in commit.segments.iter().enumerate() {
                    let idx = first_idx
                        .checked_add(position as i64)
                        .ok_or_else(|| invalid("segment index overflow"))?;
                    changed_segments = true;
                    let row = &segment.row;
                    tx.execute("INSERT INTO segments(id,transcript_id,meeting_id,idx,start_ms,end_ms,channel,speaker_label,person_id,text,confidence,is_final,words) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12)",
                        params![segment.id,transcript,meeting_id,idx,row.start_ms,row.end_ms,row.channel,row.speaker_label,row.person_id,row.text,row.confidence,row.words])?;
                }
                tx.execute("INSERT INTO speech_receipts(id,meeting_id,transcript_id,epoch_id,from_sample,through_sample,payload_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![id,meeting_id,transcript,epoch_id(session,&source.source,commit.epoch),commit.from_sample,commit.through_sample,payload])?;
                cursor = commit.through_sample;
            }
        }
        // All old indices become negative before assigning nonnegative ordinals,
        // preserving the UNIQUE constraint while ordering interleaved sources.
        if changed_segments {
            tx.execute(
                "UPDATE segments SET idx=-idx-1 WHERE transcript_id=?1",
                [&transcript],
            )?;
            tx.execute("WITH ordered AS (SELECT id,ROW_NUMBER() OVER(ORDER BY start_ms,end_ms,channel,id)-1 AS ordinal FROM segments WHERE transcript_id=?1) UPDATE segments SET idx=(SELECT ordinal FROM ordered WHERE ordered.id=segments.id) WHERE transcript_id=?1", [&transcript])?;
        }
        tx.commit()?;
        Ok(meeting_id)
    }
}

fn validate(sources: &[SpeechSource]) -> Result<()> {
    if sources.is_empty() || sources.len() > 2 {
        return Err(invalid("expected one or two source journals"));
    }
    let mut identities = HashSet::new();
    let mut segment_ids = HashSet::new();
    for source in sources {
        if !matches!(source.source.as_str(), "mic" | "system")
            || !identities.insert(&source.source)
            || source.epochs.is_empty()
        {
            return Err(invalid("invalid or duplicate source journal"));
        }
        let mut cursor = source.epochs[0].from_sample;
        for (index, epoch) in source.epochs.iter().enumerate() {
            if epoch.number != index as i64
                || epoch.from_sample < 0
                || epoch.from_sample < cursor
                || epoch.provider.is_empty()
                || epoch.provider.len() > 128
                || epoch.model.is_empty()
                || epoch.model.len() > 512
                || epoch.revision.as_ref().is_some_and(|r| r.len() > 512)
                || (index == 0) != epoch.reason.is_none()
                || epoch
                    .reason
                    .as_ref()
                    .is_some_and(|r| r.is_empty() || r.len() > 128)
                || serde_json::to_vec(&epoch.capabilities)
                    .map_err(|_| invalid("invalid capabilities"))?
                    .len()
                    > 65_536
            {
                return Err(invalid("invalid provider epoch history"));
            }
            cursor = epoch.from_sample;
        }
        cursor = source.epochs[0].from_sample;
        for commit in &source.commits {
            let active = source
                .epochs
                .iter()
                .rev()
                .find(|e| e.from_sample <= commit.from_sample)
                .ok_or_else(|| invalid("missing provider epoch"))?;
            let next = source.epochs.iter().find(|e| e.number > commit.epoch);
            if commit.from_sample != cursor
                || commit.through_sample <= cursor
                || active.number != commit.epoch
                || next.is_some_and(|e| commit.through_sample > e.from_sample)
            {
                return Err(invalid(
                    "speech receipt must extend its source epoch contiguously",
                ));
            }
            for segment in &commit.segments {
                let row = &segment.row;
                if segment.id.is_empty()
                    || segment.id.len() > 512
                    || !segment_ids.insert(&segment.id)
                    || !matches!(segment.timestamp_source.as_str(), "provider" | "estimated")
                    || row.channel != source.source
                    || !row.is_final
                    || row.start_ms < commit.from_sample / 16
                    || row.end_ms < row.start_ms
                    || row.end_ms > commit.through_sample / 16
                    || row.confidence.is_some_and(|c| !c.is_finite())
                {
                    return Err(invalid("invalid speech segment identity or coverage"));
                }
            }
            if receipt_payload(commit)?.len() > 1_048_576 {
                return Err(invalid("speech receipt exceeds journal limit"));
            }
            cursor = commit.through_sample;
        }
        if source.epochs.iter().any(|e| {
            e.from_sample != source.epochs[0].from_sample
                && !source
                    .commits
                    .iter()
                    .any(|c| c.through_sample == e.from_sample)
        }) {
            return Err(invalid("provider switch is not a committed boundary"));
        }
    }
    Ok(())
}
