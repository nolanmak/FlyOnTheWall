//! Durable per-source transcript coverage. All operations perform blocking IO.
//!
//! Audio is synced before its transcript/coverage record. A switch is synced
//! before its new epoch can publish. Recovery refuses complete corrupt records
//! and repairs only a torn last line. Callers publish only `New` commits; saved
//! stable segment identities support replay into the meeting database.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fotw_pipeline::replay::{ReplayReader, ReplaySource};
use fotw_stt::{Source, SttCapabilities, TranscriptSegment};
use serde::{Deserialize, Serialize};

const MAX_RECORD_BYTES: usize = 1_048_576;

/// Model and capabilities associated with one provider epoch. Never credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineDescriptor {
    /// Provider identifier recorded on each transcript segment.
    pub provider: String,
    /// Model identifier recorded on each transcript segment.
    pub model: String,
    /// Installed model revision/checksum, when known.
    pub revision: Option<String>,
    /// Capabilities in effect during this epoch.
    pub capabilities: SttCapabilities,
}

/// Why the session moved to another engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchReason {
    /// Cloud credentials were refused.
    Auth,
    /// Cloud quota is exhausted.
    Quota,
    /// Reconnect attempts or elapsed retry time exhausted their budget.
    TransportBudget,
    /// Rate/concurrency pressure exceeds the bounded wait budget.
    RateLimitBudget,
    /// Speech continued without a provider-confirmed final.
    SpeechStall,
    /// The user explicitly selected another engine.
    Manual,
    /// Recovery resumes pending local work after a process restart.
    ResumePending,
    /// Finish audio not confirmed by the cloud before the bounded Stop drain.
    FinishingTail,
}

/// A provider epoch begins at the last durably committed audio boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEpoch {
    /// Monotonic within one source of one session.
    pub number: u64,
    /// Absolute sample on the mono 16 kHz session clock.
    pub from_sample: u64,
    /// `None` only for the initial engine.
    pub reason: Option<SwitchReason>,
    /// Model/capabilities used for this interval.
    pub engine: EngineDescriptor,
}

/// One atomic transcript and coverage commit, retained for idempotent recovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedAudio {
    /// Provider epoch that completed inference.
    pub epoch: u64,
    /// Inclusive absolute audio sample.
    pub from_sample: u64,
    /// Exclusive absolute audio sample, including acknowledged silence.
    pub through_sample: u64,
    /// Finals for this interval. Empty only when inference confirmed silence.
    pub segments: Vec<TranscriptSegment>,
}

/// Whether a caller should publish the commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Audio and transcript were durably committed by this call.
    New,
    /// This exact receipt was already saved. Do not publish it again.
    AlreadyCommitted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: u32,
    session_id: String,
    source: Source,
    first_sample: u64,
    initial: EngineDescriptor,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Entry {
    Opened { header: Header },
    Switched { epoch: ProviderEpoch },
    Committed { commit: CommittedAudio },
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn replay_source(source: Source) -> ReplaySource {
    match source {
        Source::System => ReplaySource::System,
        Source::Mic => ReplaySource::Mic,
    }
}
fn journal_path(dir: &Path, source: Source) -> PathBuf {
    dir.join(match source {
        Source::System => "system.stt-coverage.jsonl",
        Source::Mic => "mic.stt-coverage.jsonl",
    })
}
struct OwnerLock(File);
impl Drop for OwnerLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
fn lock(path: &Path) -> io::Result<OwnerLock> {
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("jsonl.lock"))?;
    owner.try_lock().map_err(io::Error::from)?;
    Ok(OwnerLock(owner))
}
fn validate_engine(engine: &EngineDescriptor) -> io::Result<()> {
    if engine.provider.is_empty()
        || engine.provider.len() > 128
        || engine.model.is_empty()
        || engine.model.len() > 512
        || engine.revision.as_ref().is_some_and(|r| r.len() > 512)
    {
        return Err(invalid("invalid provider identity"));
    }
    Ok(())
}

// Bound serialization as well as recovery reads: a malformed provider result
// must not allocate a whole meeting's worth of JSON before being rejected.
struct RecordBuffer(Vec<u8>);
impl Write for RecordBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (MAX_RECORD_BYTES - 1).saturating_sub(self.0.len()) {
            return Err(invalid("coverage record exceeds size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode(entry: &Entry) -> io::Result<Vec<u8>> {
    let mut buffer = RecordBuffer(Vec::new());
    serde_json::to_writer(&mut buffer, entry)?;
    buffer.0.push(b'\n');
    Ok(buffer.0)
}
fn read_line(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    Read::by_ref(reader)
        .take(MAX_RECORD_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if line.len() > MAX_RECORD_BYTES {
        return Err(invalid("coverage record exceeds size limit"));
    }
    Ok(line)
}

/// Exclusive owner of one source's append-only coverage journal.
///
/// The journal is session data, not diagnostic logging. Keep it and canonical
/// replay audio until pending transcription is resolved. Never call from an
/// audio callback or async executor; the handoff worker owns blocking IO.
pub struct CoverageJournal {
    _owner: OwnerLock,
    file: File,
    path: PathBuf,
    audio: ReplayReader,
    header: Header,
    epoch: ProviderEpoch,
    commits: Vec<CommittedAudio>,
    ids: HashSet<String>,
    committed: u64,
    failed: bool,
}

impl CoverageJournal {
    /// Start a journal beside an already-created canonical replay stream.
    pub fn create(
        dir: &Path,
        session_id: &str,
        source: Source,
        initial: EngineDescriptor,
    ) -> io::Result<Self> {
        validate_engine(&initial)?;
        let audio = ReplayReader::open(dir, session_id, replay_source(source))?;
        let header = Header {
            schema: 1,
            session_id: session_id.into(),
            source,
            first_sample: audio.first_sample(),
            initial,
        };
        let path = journal_path(dir, source);
        let owner = lock(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut journal = Self::from_header(owner, file, path, audio, header);
        let initialized = journal
            .append(&Entry::Opened {
                header: journal.header.clone(),
            })
            .and_then(|()| {
                #[cfg(unix)]
                File::open(dir)?.sync_all()?;
                Ok(())
            });
        if let Err(error) = initialized {
            let path = journal.path.clone();
            drop(journal);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(journal)
    }

    /// Recover committed payloads and the current provider epoch after a crash.
    /// Only an incomplete final line is truncated, after prior records validate.
    pub fn open(dir: &Path, session_id: &str, source: Source) -> io::Result<Self> {
        let audio = ReplayReader::open(dir, session_id, replay_source(source))?;
        let path = journal_path(dir, source);
        let owner = lock(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let mut reader = BufReader::new(file.try_clone()?);
        let first = read_line(&mut reader)?;
        if first.last() != Some(&b'\n') {
            return Err(invalid("missing coverage header"));
        }
        let Entry::Opened { header } = serde_json::from_slice(&first)? else {
            return Err(invalid("missing coverage header"));
        };
        if header.schema != 1
            || header.session_id != session_id
            || header.source != source
            || header.first_sample != audio.first_sample()
        {
            return Err(invalid("coverage identity mismatch"));
        }
        validate_engine(&header.initial)?;
        let mut journal = Self::from_header(owner, file, path, audio, header);
        let mut valid_bytes = first.len() as u64;
        let mut torn = false;
        loop {
            let line = read_line(&mut reader)?;
            if line.is_empty() {
                break;
            }
            if line.last() != Some(&b'\n') {
                torn = true;
                break;
            }
            match serde_json::from_slice(&line)? {
                Entry::Opened { .. } => return Err(invalid("duplicate coverage header")),
                Entry::Switched { epoch } => {
                    journal.validate_switch(&epoch)?;
                    journal.epoch = epoch;
                }
                Entry::Committed { commit } => {
                    journal.validate_commit(&commit)?;
                    journal.accept(commit);
                }
            }
            valid_bytes += line.len() as u64;
        }
        drop(reader);
        if torn {
            journal.file.set_len(valid_bytes)?;
            journal.file.sync_data()?;
        }
        journal.file.seek(SeekFrom::End(0))?;
        Ok(journal)
    }

    fn from_header(
        owner: OwnerLock,
        file: File,
        path: PathBuf,
        audio: ReplayReader,
        header: Header,
    ) -> Self {
        Self {
            _owner: owner,
            file,
            path,
            audio,
            epoch: ProviderEpoch {
                number: 0,
                from_sample: header.first_sample,
                reason: None,
                engine: header.initial.clone(),
            },
            committed: header.first_sample,
            header,
            commits: Vec::new(),
            ids: HashSet::new(),
            failed: false,
        }
    }

    /// Current source coverage on the absolute sample clock.
    #[must_use]
    pub const fn committed_through(&self) -> u64 {
        self.committed
    }
    /// Engine and fencing token to use for the next uncommitted interval.
    #[must_use]
    pub const fn current_epoch(&self) -> &ProviderEpoch {
        &self.epoch
    }
    /// Previously committed transcript payloads, with stable segment identities.
    #[must_use]
    pub fn commits(&self) -> &[CommittedAudio] {
        &self.commits
    }
    /// Journal location, kept with this session's audio.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sync audio and one exact transcript receipt before the caller publishes.
    /// A caller must independently establish completed inference; a transport
    /// send or a forced partial is not an audio coverage acknowledgement.
    pub fn commit(
        &mut self,
        epoch: u64,
        from_sample: u64,
        through_sample: u64,
        segments: Vec<TranscriptSegment>,
    ) -> io::Result<CommitOutcome> {
        self.healthy()?;
        if epoch != self.epoch.number {
            return Err(invalid("stale provider epoch"));
        }
        let commit = CommittedAudio {
            epoch,
            from_sample,
            through_sample,
            segments,
        };
        if let Some(prior) = self
            .commits
            .iter()
            .find(|c| c.from_sample == from_sample && c.through_sample == through_sample)
        {
            return if prior == &commit {
                Ok(CommitOutcome::AlreadyCommitted)
            } else {
                Err(invalid("conflicting committed interval"))
            };
        }
        self.validate_commit(&commit)?;
        self.audio.sync_through(through_sample)?;
        self.append(&Entry::Committed {
            commit: commit.clone(),
        })?;
        self.accept(commit);
        Ok(CommitOutcome::New)
    }

    /// Persist a new fencing token, starting at the existing committed boundary.
    pub fn switch(
        &mut self,
        engine: EngineDescriptor,
        reason: SwitchReason,
    ) -> io::Result<ProviderEpoch> {
        self.healthy()?;
        let epoch = ProviderEpoch {
            number: self
                .epoch
                .number
                .checked_add(1)
                .ok_or_else(|| invalid("provider epoch overflow"))?,
            from_sample: self.committed,
            reason: Some(reason),
            engine,
        };
        self.validate_switch(&epoch)?;
        self.append(&Entry::Switched {
            epoch: epoch.clone(),
        })?;
        self.epoch = epoch.clone();
        Ok(epoch)
    }

    fn validate_switch(&self, epoch: &ProviderEpoch) -> io::Result<()> {
        validate_engine(&epoch.engine)?;
        if self.epoch.number.checked_add(1) != Some(epoch.number)
            || epoch.from_sample != self.committed
            || epoch.reason.is_none()
        {
            return Err(invalid("invalid provider epoch transition"));
        }
        Ok(())
    }
    fn validate_commit(&mut self, commit: &CommittedAudio) -> io::Result<()> {
        if commit.epoch != self.epoch.number
            || commit.from_sample != self.committed
            || commit.through_sample <= commit.from_sample
        {
            return Err(invalid(
                "coverage must extend the current epoch contiguously",
            ));
        }
        // This checks availability, not durability; commit() syncs before append.
        self.audio.read_from(commit.through_sample, 1)?;
        let mut ids = HashSet::new();
        for segment in &commit.segments {
            if segment.id.is_empty()
                || !ids.insert(&segment.id)
                || self.ids.contains(&segment.id)
                || !segment.is_final
                || segment.session_id != self.header.session_id
                || segment.source != self.header.source
                || segment.provider != self.epoch.engine.provider
                || segment.model != self.epoch.engine.model
                || segment.start_ms < commit.from_sample / 16
                || segment.end_ms < segment.start_ms
                || segment.end_ms > commit.through_sample / 16
                || segment.confidence.is_some_and(|c| !c.is_finite())
                || segment.words.iter().any(|word| {
                    word.start_ms < segment.start_ms
                        || word.end_ms > segment.end_ms
                        || word.end_ms < word.start_ms
                        || word.confidence.is_some_and(|c| !c.is_finite())
                })
            {
                return Err(invalid(
                    "segment identity or interval does not match its coverage",
                ));
            }
        }
        Ok(())
    }
    fn accept(&mut self, commit: CommittedAudio) {
        self.committed = commit.through_sample;
        self.ids
            .extend(commit.segments.iter().map(|s| s.id.clone()));
        self.commits.push(commit);
    }
    fn append(&mut self, entry: &Entry) -> io::Result<()> {
        self.healthy()?;
        let bytes = encode(entry)?;
        if let Err(error) = self
            .file
            .write_all(&bytes)
            .and_then(|()| self.file.sync_data())
        {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }
    fn healthy(&self) -> io::Result<()> {
        if self.failed {
            Err(invalid("coverage journal failed; recovery required"))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod owner_release_tests {
    use super::*;

    #[test]
    fn a_duplicate_descriptor_does_not_retain_departed_journal_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let _audio =
            fotw_pipeline::replay::ReplayWriter::create(dir.path(), "s", ReplaySource::System, 0)
                .unwrap();
        let journal = CoverageJournal::create(
            dir.path(),
            "s",
            Source::System,
            EngineDescriptor {
                provider: "fixture".into(),
                model: "fixture".into(),
                revision: None,
                capabilities: SttCapabilities::deepgram(),
            },
        )
        .unwrap();
        let inherited = journal._owner.0.try_clone().unwrap();
        drop(journal);
        let recovered = CoverageJournal::open(dir.path(), "s", Source::System);
        assert!(
            recovered.is_ok(),
            "departed journal owner retained lock: {:?}",
            recovered.as_ref().err()
        );
        drop(recovered);
        drop(inherited);
    }
}
