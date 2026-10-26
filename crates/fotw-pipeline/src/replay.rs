//! Disk-backed canonical transcription audio for provider handoffs.
//!
//! This is the **derived** mono 16 kHz PCM16 stream after resampling and gap
//! padding. Durable handoff preserves microphone speech before echo suppression. It supplements the original capture WAL; rebuilding
//! this stream from native-format audio can change both its contents and clock.
//! Each leg has its own absolute session sample anchor. Reads are bounded to
//! one second and use independent file descriptions, so catchup cannot move a
//! live writer's offset or allocate an outage's worth of memory.
//!
//! All operations are blocking disk IO. Callers must run them on the capture
//! pump or a blocking worker, never the audio callback or an async executor.
//! Creation is exclusive; this module never overwrites an existing recording.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"FOTWRPL1";
const MAX_HEADER_BYTES: usize = 4096;
const SAMPLE_RATE: u32 = 16_000;
static NEXT_COMPLETION: AtomicU64 = AtomicU64::new(0);

/// Largest catchup read: one second of mono PCM16, independent of outage length.
pub const MAX_READ_SAMPLES: usize = SAMPLE_RATE as usize;

/// Capture leg. Kept separate on disk even when both providers are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplaySource {
    /// Other participants, captured from system output.
    System,
    /// Local microphone, preserving its original speech in durable handoff mode.
    Mic,
}

impl ReplaySource {
    fn prefix_projected_path(self, dir: &Path) -> PathBuf {
        dir.join(match self {
            Self::System => "system.stt-prefix-projected.json",
            Self::Mic => "mic.stt-prefix-projected.json",
        })
    }
    fn projected_path(self, dir: &Path) -> PathBuf {
        dir.join(match self {
            Self::System => "system.stt-projected.json",
            Self::Mic => "mic.stt-projected.json",
        })
    }
    fn coverage_path(self, dir: &Path) -> PathBuf {
        dir.join(match self {
            Self::System => "system.stt-coverage.jsonl",
            Self::Mic => "mic.stt-coverage.jsonl",
        })
    }
    fn lock_coverage(self, dir: &Path) -> io::Result<OwnerLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.coverage_path(dir).with_extension("jsonl.lock"))?;
        file.try_lock().map_err(io::Error::from)?;
        Ok(OwnerLock(file))
    }

    fn sealed_path(self, dir: &Path) -> PathBuf {
        dir.join(match self {
            Self::System => "system.stt-sealed.json",
            Self::Mic => "mic.stt-sealed.json",
        })
    }

    fn completion_path(self, dir: &Path) -> PathBuf {
        dir.join(match self {
            Self::System => "system.stt-complete.json",
            Self::Mic => "mic.stt-complete.json",
        })
    }
    fn path(self, dir: &Path) -> PathBuf {
        dir.join(match self {
            Self::System => "system.stt16",
            Self::Mic => "mic.stt16",
        })
    }

    fn lock(self, dir: &Path) -> io::Result<OwnerLock> {
        let path = self.path(dir).with_extension("stt16.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        // Lock a sidecar, not PCM: on Windows a byte-range lock on PCM would
        // also prevent independent readers from reading the locked bytes.
        file.try_lock().map_err(io::Error::from)?;
        Ok(OwnerLock(file))
    }
}

struct OwnerLock(File);

impl Drop for OwnerLock {
    fn drop(&mut self) {
        // A process spawn may briefly inherit this open file description
        // before exec closes CLOEXEC descriptors. Closing our copy alone can
        // leave flock held by that copy; explicitly release logical ownership.
        let _ = self.0.unlock();
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    session_id: String,
    source: ReplaySource,
    sample_rate: u32,
    first_sample: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    processing_id: Option<String>,
}

/// Exclusive offline restoration of one canonical source. The caller must
/// validate the original capture, raw-processing recipe and coverage
/// journal before publishing. No ordinary append or completion capability is
/// exposed, and an existing malformed header is never replaced.
pub struct ReconstructionOwner {
    _owner: OwnerLock,
    dir: PathBuf,
    expected: Header,
    original: Option<ReplayReader>,
}
impl ReconstructionOwner {
    /// Reserve the source's normal sidecar lease without creating an audio
    /// file. Missing input stays absent until a validated candidate is ready.
    pub fn reserve(
        dir: &Path,
        session: &str,
        source: ReplaySource,
        first_sample: u64,
    ) -> io::Result<Self> {
        if session.is_empty() || session.len() > 512 {
            return Err(invalid("invalid restoration session"));
        }
        let owner = source.lock(dir)?;
        let path = source.path(dir);
        let original = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_file() => Some(ReplayReader::open(dir, session, source)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
            _ => return Err(invalid("restoration source is not an ordinary file")),
        };
        let expected = Header {
            processing_id: None,
            session_id: session.into(),
            source,
            sample_rate: SAMPLE_RATE,
            first_sample,
        };
        if original.as_ref().is_some_and(|r| r.header != expected) {
            return Err(invalid("retained canonical identity differs"));
        }
        Ok(Self {
            _owner: owner,
            dir: dir.into(),
            expected,
            original,
        })
    }
    /// Observe the complete retained prefix; absence is distinct from empty.
    pub fn retained_end(&self) -> io::Result<Option<u64>> {
        self.original
            .as_ref()
            .map(ReplayReader::complete_prefix_end)
            .transpose()
    }
    /// Validate existing proof files against healthy retained input, without
    /// modifying them. Damaged input is instead checked against the candidate.
    pub fn validate_current_markers(&self) -> io::Result<()> {
        validate_reconstruction_markers(
            &self.dir,
            self.original
                .as_ref()
                .ok_or_else(|| invalid("canonical source is absent"))?,
        )
    }
    /// Check identity, alignment, all retained samples and existing proof
    /// boundaries. The candidate never grants new completion authority.
    pub fn validate_candidate(
        &mut self,
        candidate: &mut ReplayReader,
        stop: &AtomicBool,
    ) -> io::Result<u64> {
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        checked_candidate_path(candidate, stop)?;
        if candidate.header != self.expected {
            return Err(invalid("candidate restoration identity differs"));
        }
        let target = candidate.saved_end()?;
        if let Some(original) = &mut self.original {
            let end = original.complete_prefix_end()?;
            if end > target {
                return Err(invalid("candidate would shorten retained audio"));
            }
            let mut at = original.first_sample();
            while at < end {
                if stop.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let count = (end - at).min(MAX_READ_SAMPLES as u64) as usize;
                let retained = original.read_from(at, count)?;
                if retained.len() != count || candidate.read_from(at, count)? != retained {
                    return Err(invalid(
                        "candidate would replace retained canonical samples",
                    ));
                }
                at += count as u64;
            }
        }
        validate_reconstruction_markers(&self.dir, candidate)?;
        Ok(target)
    }
    /// Restore only validated missing bytes, checking cancellation between
    /// bounded copies. A missing file is published with a same-filesystem
    /// create-new hard link, so a concurrently appearing path is never replaced.
    /// Existing proof files and the original header are left unchanged.
    pub fn restore(&mut self, candidate: &mut ReplayReader, stop: &AtomicBool) -> io::Result<u64> {
        let target = self.validate_candidate(candidate, stop)?;
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if let Some(original) = &mut self.original {
            let mut at = original.complete_prefix_end()?;
            original
                .file
                .set_len(original.audio_offset + (at - original.first_sample()) * 2)?;
            original.file.seek(SeekFrom::End(0))?;
            while at < target {
                if stop.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let count = (target - at).min(MAX_READ_SAMPLES as u64) as usize;
                let pcm = candidate.read_from(at, count)?;
                if pcm.len() != count {
                    return Err(invalid("candidate shortened during restoration"));
                }
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                original.file.write_all(&bytes)?;
                at += count as u64;
            }
            original.file.sync_all()?;
        } else {
            candidate.file.sync_all()?;
            let path = checked_candidate_path(candidate, stop)?;
            std::fs::hard_link(path, self.expected.source.path(&self.dir))?;
            self.original = Some(ReplayReader::open(
                &self.dir,
                &self.expected.session_id,
                self.expected.source,
            )?);
        }
        sync_directory(&self.dir)?;
        Ok(target)
    }
}

fn checked_candidate_path(candidate: &mut ReplayReader, stop: &AtomicBool) -> io::Result<PathBuf> {
    let path = candidate.header.source.path(&candidate.dir);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() {
        return Err(invalid("candidate path is not an ordinary file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let retained = candidate.file.metadata()?;
        if metadata.dev() != retained.dev() || metadata.ino() != retained.ino() {
            return Err(invalid(
                "candidate path no longer refers to validated input",
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let mut current = ReplayReader::open(
            &candidate.dir,
            &candidate.header.session_id,
            candidate.header.source,
        )?;
        if current.header != candidate.header || current.saved_end()? != candidate.saved_end()? {
            return Err(invalid("candidate path identity changed"));
        }
        let end = candidate.saved_end()?;
        let mut at = candidate.first_sample();
        while at < end {
            if stop.load(Ordering::Acquire) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = (end - at).min(MAX_READ_SAMPLES as u64) as usize;
            if current.read_from(at, count)? != candidate.read_from(at, count)? {
                return Err(invalid("candidate path content changed"));
            }
            at += count as u64;
        }
    }
    if stop.load(Ordering::Acquire) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    Ok(path)
}

fn validate_reconstruction_markers(dir: &Path, candidate: &ReplayReader) -> io::Result<()> {
    let source = candidate.header.source;
    // Retained input can have an incomplete final sample. Proofs describe
    // complete samples only; fresh candidates are checked for alignment by
    // validate_candidate before reaching this shared marker check.
    let end = candidate.complete_prefix_end()?;
    let matches = |b: &Completion, full: bool| {
        b.schema == 1
            && b.session_id == candidate.header.session_id
            && b.source == source
            && b.first_sample == candidate.first_sample()
            && b.through_sample >= candidate.first_sample()
            && if full {
                b.through_sample == end
            } else {
                b.through_sample <= end
            }
    };
    for path in [
        source.sealed_path(dir),
        source.completion_path(dir),
        source.projected_path(dir),
        source.prefix_projected_path(dir),
    ] {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_file() => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            _ => return Err(invalid("restoration proof is not an ordinary file")),
        }
    }
    for path in [source.sealed_path(dir), source.completion_path(dir)] {
        if read_marker::<Completion>(path)?.is_some_and(|b| !matches(&b, true)) {
            return Err(invalid("restoration conflicts with frozen source proof"));
        }
    }
    if read_marker::<ProjectionProof>(source.projected_path(dir))?
        .is_some_and(|p| !matches(&p.boundary, true) || p.journal_bytes == 0)
    {
        return Err(invalid("restoration projection identity differs"));
    }
    if read_marker::<PrefixProjection>(source.prefix_projected_path(dir))?.is_some_and(|p| {
        p.scope != "saved-prefix-projection" || !matches(&p.boundary, false) || p.journal_bytes == 0
    }) {
        return Err(invalid("restoration prefix proof differs"));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    schema: u8,
    session_id: String,
    source: ReplaySource,
    first_sample: u64,
    through_sample: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionProof {
    boundary: Completion,
    journal_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrefixProjection {
    scope: String,
    boundary: Completion,
    journal_bytes: u64,
}

fn write_marker<T: Serialize>(dir: &Path, path: PathBuf, marker: &T) -> io::Result<()> {
    let temporary = path.with_extension(format!(
        "part-{}-{}",
        std::process::id(),
        NEXT_COMPLETION.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(marker)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &path)?;
        sync_directory(dir)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn read_marker<T: serde::de::DeserializeOwned>(path: PathBuf) -> io::Result<Option<T>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_HEADER_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_HEADER_BYTES {
        return Err(invalid("replay boundary proof exceeds size limit"));
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn sync_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Held across the atomic retirement rename, so another process cannot resume
/// appending between completion validation and removal. Drop before unlinking.
pub(crate) struct ReplayRetirementGuard {
    _owners: Vec<OwnerLock>,
}

pub(crate) fn retirement_guard(
    dir: &Path,
    session_id: &str,
) -> io::Result<Option<ReplayRetirementGuard>> {
    let mut owners = Vec::new();
    for source in [ReplaySource::System, ReplaySource::Mic] {
        if !source.path(dir).try_exists()? {
            if source.completion_path(dir).try_exists()? || source.sealed_path(dir).try_exists()? {
                return Err(invalid("completion proof has no replay audio"));
            }
            continue;
        }
        let owner = match source.lock(dir) {
            Ok(owner) => owner,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        let journal_owner = match source.lock_coverage(dir) {
            Ok(owner) => owner,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        let reader = ReplayReader::open(dir, session_id, source)?;
        if !reader.is_complete()? || !reader.is_projected()? {
            return Ok(None);
        }
        owners.push(owner);
        owners.push(journal_owner);
    }
    Ok(Some(ReplayRetirementGuard { _owners: owners }))
}

fn valid_processing_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Sole append owner for one leg's canonical stream.
pub struct ReplayWriter {
    _owner: OwnerLock,
    file: File,
    end_sample: u64,
    failed: bool,
    dir: PathBuf,
    source: ReplaySource,
    session_id: String,
    sealed: bool,
    prefix_only: bool,
}

impl ReplayWriter {
    /// Create a new stream inside an existing session directory.
    ///
    /// `first_sample` is on the absolute 16 kHz session clock, including any
    /// delay before this leg started. A second owner is refused with no changes.
    pub fn create(
        dir: &Path,
        session_id: &str,
        source: ReplaySource,
        first_sample: u64,
    ) -> io::Result<Self> {
        Self::create_inner(dir, session_id, source, first_sample, None)
    }
    /// Declare an immutable processing configuration fingerprint in the audio
    /// header, so losing a sidecar cannot reclassify processed audio as legacy.
    pub fn create_with_processing(
        dir: &Path,
        session_id: &str,
        source: ReplaySource,
        first_sample: u64,
        processing_id: &str,
    ) -> io::Result<Self> {
        if !valid_processing_id(processing_id) {
            return Err(invalid("invalid processing fingerprint"));
        }
        Self::create_inner(
            dir,
            session_id,
            source,
            first_sample,
            Some(processing_id.into()),
        )
    }
    fn create_inner(
        dir: &Path,
        session_id: &str,
        source: ReplaySource,
        first_sample: u64,
        processing_id: Option<String>,
    ) -> io::Result<Self> {
        if session_id.is_empty() || session_id.len() > 512 {
            return Err(invalid("invalid replay session identity"));
        }
        let header = serde_json::to_vec(&Header {
            processing_id,
            session_id: session_id.to_owned(),
            source,
            sample_rate: SAMPLE_RATE,
            first_sample,
        })?;
        if header.len() > MAX_HEADER_BYTES {
            return Err(invalid("replay header too large"));
        }
        let path = source.path(dir);
        let owner = source.lock(dir)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let initialize = (|| {
            file.write_all(MAGIC)?;
            file.write_all(&(header.len() as u32).to_le_bytes())?;
            file.write_all(&header)?;
            file.sync_all()?;
            // Persist the directory entry too, before a checkpoint can refer
            // to this stream. Windows does not support opening directories
            // through std::fs::File; sync_all above still flushes its file.
            #[cfg(unix)]
            File::open(dir)?.sync_all()?;
            Ok::<_, io::Error>(())
        })();
        if let Err(error) = initialize {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(Self {
            _owner: owner,
            file,
            end_sample: first_sample,
            failed: false,
            dir: dir.to_owned(),
            source,
            session_id: session_id.to_owned(),
            sealed: false,
            prefix_only: false,
        })
    }

    /// Resume after a process crash, under an exclusive, nonblocking OS lock.
    ///
    /// Complete samples are preserved; a trailing half-sample is truncated
    /// before any new PCM is appended. Identity/format validation precedes all
    /// mutation. A still-live writer causes an immediate error, never a wait.
    pub fn resume(dir: &Path, session_id: &str, source: ReplaySource) -> io::Result<Self> {
        Self::resume_validated(dir, session_id, source, |_| Ok(()))
    }

    /// Validate caller-owned processing evidence under the source lease before
    /// any torn-sample repair or append capability is granted.
    pub fn resume_validated(
        dir: &Path,
        session_id: &str,
        source: ReplaySource,
        validate: impl FnOnce(&mut ReplayReader) -> io::Result<()>,
    ) -> io::Result<Self> {
        let owner = source.lock(dir)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(source.path(dir))?;
        let mut reader = ReplayReader::from_file(dir, file, session_id, source)?;
        validate(&mut reader)?;
        let sealed = reader.sealed_end()?.is_some();
        let length = reader.file.metadata()?.len();
        let audio_bytes = length
            .checked_sub(reader.audio_offset)
            .ok_or_else(|| invalid("replay header truncated"))?;
        let end_sample = reader
            .header
            .first_sample
            .checked_add(audio_bytes / 2)
            .ok_or_else(|| invalid("replay sample clock overflow"))?;
        if audio_bytes % 2 != 0 {
            reader.file.set_len(length - 1)?;
            reader.file.sync_data()?;
        }
        reader.file.seek(SeekFrom::End(0))?;
        Ok(Self {
            _owner: owner,
            file: reader.file,
            end_sample,
            failed: false,
            dir: dir.to_owned(),
            source,
            session_id: session_id.to_owned(),
            sealed,
            prefix_only: false,
        })
    }

    /// Own an existing canonical prefix without repairing its trailing bytes
    /// or granting append/seal capability. The complete prefix is synced;
    /// its observed end does not certify capture or transcription completion.
    pub fn resume_prefix(dir: &Path, session_id: &str, source: ReplaySource) -> io::Result<Self> {
        Self::resume_prefix_validated(dir, session_id, source, |_| Ok(()))
    }

    /// Validate caller-owned evidence under the prefix lease without truncating
    /// any bytes or granting append/seal capability.
    pub fn resume_prefix_validated(
        dir: &Path,
        session_id: &str,
        source: ReplaySource,
        validate: impl FnOnce(&mut ReplayReader) -> io::Result<()>,
    ) -> io::Result<Self> {
        let owner = source.lock(dir)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(source.path(dir))?;
        let mut reader = ReplayReader::from_file(dir, file, session_id, source)?;
        validate(&mut reader)?;
        let sealed = reader.sealed_end()?.is_some();
        let end_sample = reader.available_end()?;
        reader.file.sync_data()?;
        Ok(Self {
            _owner: owner,
            file: reader.file,
            end_sample,
            failed: false,
            dir: dir.to_owned(),
            source,
            session_id: session_id.to_owned(),
            sealed,
            prefix_only: true,
        })
    }

    /// Append the exact PCM sent to STT, including silence. No audio deduplication.
    ///
    /// A disk error poisons this owner: subsequent writes cannot pretend a
    /// partially written chunk was a complete interval. Original capture audio
    /// must continue independently and surface an incomplete-transcript error.
    pub fn append(&mut self, pcm: &[i16]) -> io::Result<()> {
        self.check_healthy()?;
        if self.sealed || self.prefix_only {
            return Err(invalid("replay input is permanently sealed"));
        }
        if pcm.is_empty() {
            return Ok(());
        }
        let next = self
            .end_sample
            .checked_add(pcm.len() as u64)
            .ok_or_else(|| invalid("replay sample clock overflow"))?;
        // Invalidate before accepting any new audio. A stale completion must
        // never authorize retirement of an extended stream, including after a crash.
        for proof in [
            self.source.completion_path(&self.dir),
            self.source.projected_path(&self.dir),
        ] {
            match std::fs::remove_file(proof) {
                Ok(()) => {
                    if let Err(error) = sync_directory(&self.dir) {
                        self.failed = true;
                        return Err(error);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        let mut bytes = [0u8; 2048];
        for chunk in pcm.chunks(bytes.len() / 2) {
            for (sample, encoded) in chunk.iter().zip(bytes.chunks_exact_mut(2)) {
                encoded.copy_from_slice(&sample.to_le_bytes());
            }
            if let Err(error) = self.file.write_all(&bytes[..chunk.len() * 2]) {
                self.failed = true;
                return Err(error);
            }
        }
        self.end_sample = next;
        Ok(())
    }

    /// Flush audio to storage before persisting a checkpoint referring to it.
    /// Returns the absolute end of the successfully appended interval.
    pub fn sync(&mut self) -> io::Result<u64> {
        self.check_healthy()?;
        if let Err(error) = self.file.sync_data() {
            self.failed = true;
            return Err(error);
        }
        Ok(self.end_sample)
    }

    /// Freeze and sync the complete aligned input. The identity-bound atomic
    /// marker survives a restart; neither this owner nor a resumed owner may
    /// append afterward. This does not certify transcription completion.
    pub fn seal(&mut self) -> io::Result<u64> {
        self.check_healthy()?;
        if self.prefix_only {
            return Err(invalid("prefix ownership cannot seal original input"));
        }
        let mut reader = ReplayReader::open(&self.dir, &self.session_id, self.source)?;
        reader.sealed_end()?;
        reader.write_boundary(self.end_sample, self.source.sealed_path(&self.dir))?;
        self.sealed = true;
        Ok(self.end_sample)
    }

    /// Absolute end of successfully appended audio, not a durability promise.
    #[must_use]
    pub const fn end_sample(&self) -> u64 {
        self.end_sample
    }

    /// Validate a separately reconstructed candidate without modifying this
    /// stream. The caller must verify its raw-input processing provenance.
    /// Identity and every existing complete sample must agree; sealed input
    /// cannot be extended. Cancellation is checked between bounded reads.
    pub fn validate_reconstructed_suffix(
        &self,
        candidate: &mut ReplayReader,
        stop: &AtomicBool,
    ) -> io::Result<u64> {
        self.check_healthy()?;
        let mut original = ReplayReader::open(&self.dir, &self.session_id, self.source)?;
        if original.header != candidate.header
            || original.sealed_end()?.is_some()
            || original.complete_prefix_end()? != self.end_sample
            || candidate.complete_prefix_end()? < self.end_sample
        {
            return Err(invalid("reconstructed input identity or boundary differs"));
        }
        let mut at = original.first_sample();
        while at < self.end_sample {
            if stop.load(Ordering::Acquire) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = (self.end_sample - at).min(MAX_READ_SAMPLES as u64) as usize;
            let retained = original.read_from(at, count)?;
            if retained.len() != count || candidate.read_from(at, count)? != retained {
                return Err(invalid(
                    "reconstruction would replace retained canonical audio",
                ));
            }
            at += count as u64;
        }
        candidate.complete_prefix_end()
    }

    /// Append only a validated missing suffix while retaining this owner's
    /// lease. Existing complete samples are never overwritten. A torn half
    /// sample is removed only after validation. This grants no ordinary append,
    /// seal, completion or retirement capability to a prefix-only owner.
    pub fn restore_reconstructed_suffix(
        &mut self,
        candidate: &mut ReplayReader,
        stop: &AtomicBool,
    ) -> io::Result<u64> {
        let target = self.validate_reconstructed_suffix(candidate, stop)?;
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let original = ReplayReader::open(&self.dir, &self.session_id, self.source)?;
        self.file
            .set_len(original.audio_offset + (self.end_sample - original.first_sample()) * 2)?;
        self.file.seek(SeekFrom::End(0))?;
        while self.end_sample < target {
            if stop.load(Ordering::Acquire) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = (target - self.end_sample).min(MAX_READ_SAMPLES as u64) as usize;
            let pcm = candidate.read_from(self.end_sample, count)?;
            if pcm.len() != count {
                return Err(invalid("reconstructed input shortened"));
            }
            let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
            if let Err(error) = self.file.write_all(&bytes) {
                self.failed = true;
                return Err(error);
            }
            self.end_sample += pcm.len() as u64;
        }
        self.sync()
    }

    fn check_healthy(&self) -> io::Result<()> {
        if self.failed {
            Err(invalid("replay writer failed; recovery required"))
        } else {
            Ok(())
        }
    }
}

/// Independent read cursor for live catchup or recovery after process restart.
pub struct ReplayReader {
    file: File,
    header: Header,
    audio_offset: u64,
    dir: PathBuf,
}

impl ReplayReader {
    /// Immutable audio-processing fingerprint, absent on legacy/original input.
    #[must_use]
    pub fn processing_id(&self) -> Option<&str> {
        self.header.processing_id.as_deref()
    }
    /// Whether this already validated reader belongs to the expected source.
    #[must_use]
    pub fn has_identity(&self, session: &str, source: ReplaySource) -> bool {
        self.header.session_id == session && self.header.source == source
    }
    /// Open one track, refusing another session's audio or an unknown format.
    /// Requires write access so [`Self::sync_through`] also works on Windows.
    /// No method on this reader modifies the audio or header.
    pub fn open(dir: &Path, session_id: &str, source: ReplaySource) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(source.path(dir))?;
        Self::from_file(dir, file, session_id, source)
    }

    fn from_file(
        dir: &Path,
        mut file: File,
        session_id: &str,
        source: ReplaySource,
    ) -> io::Result<Self> {
        if !file.metadata()?.is_file() {
            return Err(invalid("replay is not a regular file"));
        }
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(invalid("unsupported replay format"));
        }
        let mut length = [0; 4];
        file.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > MAX_HEADER_BYTES {
            return Err(invalid("invalid replay header length"));
        }
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        let header: Header = serde_json::from_slice(&bytes)?;
        if header.session_id != session_id
            || header.source != source
            || header.sample_rate != SAMPLE_RATE
            || header
                .processing_id
                .as_deref()
                .is_some_and(|id| !valid_processing_id(id))
        {
            return Err(invalid("replay identity or sample rate mismatch"));
        }
        Ok(Self {
            file,
            header,
            audio_offset: 12 + length as u64,
            dir: dir.to_owned(),
        })
    }

    /// First absolute sample; the stream never implicitly starts again at zero.
    #[must_use]
    pub const fn first_sample(&self) -> u64 {
        self.header.first_sample
    }

    /// Current aligned saved end, without implying completed inference.
    pub fn saved_end(&self) -> io::Result<u64> {
        if !self.aligned()? {
            return Err(invalid("replay ends in a torn sample"));
        }
        self.available_end()
    }

    /// Complete samples currently available, excluding a trailing half-sample.
    /// This is an observation, not a seal or a transcription completion proof.
    pub fn complete_prefix_end(&self) -> io::Result<u64> {
        self.available_end()
    }

    /// Persist completion only after input is frozen and inference has durably
    /// accounted for every saved sample. Partial coverage and torn samples are
    /// refused. The caller establishes inference coverage; this verifies audio.
    pub fn mark_complete(&mut self, through_sample: u64) -> io::Result<()> {
        self.sealed_end()?;
        self.write_boundary(
            through_sample,
            self.header.source.completion_path(&self.dir),
        )
    }

    /// Persisted frozen source end, refusing altered input or another identity.
    /// Absent on older/unclosed sources; an exclusive resume owner may seal it.
    pub fn sealed_end(&self) -> io::Result<Option<u64>> {
        match self.read_boundary(self.header.source.sealed_path(&self.dir))? {
            None => Ok(None),
            Some(boundary) if self.matches_boundary(&boundary)? => {
                Ok(Some(boundary.through_sample))
            }
            Some(_) => Err(invalid(
                "sealed replay input has changed or belongs to another source",
            )),
        }
    }

    fn write_boundary(&mut self, through_sample: u64, path: PathBuf) -> io::Result<()> {
        let boundary = self.boundary(through_sample)?;
        write_marker(&self.dir, path, &boundary)
    }

    fn boundary(&mut self, through_sample: u64) -> io::Result<Completion> {
        if !self.aligned()? || through_sample != self.available_end()? {
            return Err(invalid(
                "completion must cover the entire aligned replay stream",
            ));
        }
        self.sync_through(through_sample)?;
        Ok(Completion {
            schema: 1,
            session_id: self.header.session_id.clone(),
            source: self.header.source,
            first_sample: self.first_sample(),
            through_sample,
        })
    }

    /// Acknowledge an already committed database projection of the complete
    /// source and this exact append-only journal prefix. Call only after the
    /// library transaction succeeds; growing journals invalidate the proof.
    pub fn mark_projected(&mut self, through_sample: u64, journal_bytes: u64) -> io::Result<()> {
        if !self.is_complete()? {
            return Err(invalid("inference is not complete"));
        }
        let journal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.header.source.coverage_path(&self.dir))?;
        if !journal.metadata()?.is_file() || journal.metadata()?.len() != journal_bytes {
            return Err(invalid("projection does not cover the current journal"));
        }
        journal.sync_data()?;
        let proof = ProjectionProof {
            boundary: self.boundary(through_sample)?,
            journal_bytes,
        };
        write_marker(
            &self.dir,
            self.header.source.projected_path(&self.dir),
            &proof,
        )
    }

    /// Acknowledge only a committed prefix projection while the caller owns
    /// canonical/journal leases. This separate scoped proof cannot authorize
    /// whole-input completion or retirement, including for aligned input.
    pub fn mark_prefix_projected(
        &mut self,
        through_sample: u64,
        journal_bytes: u64,
    ) -> io::Result<()> {
        self.sync_through(through_sample)?;
        let mut journal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.header.source.coverage_path(&self.dir))?;
        if journal_bytes == 0
            || !journal.metadata()?.is_file()
            || journal.metadata()?.len() < journal_bytes
        {
            return Err(invalid("prefix projection exceeds saved journal"));
        }
        journal.seek(SeekFrom::Start(journal_bytes - 1))?;
        let mut last = [0];
        journal.read_exact(&mut last)?;
        if last[0] != b'\n' {
            return Err(invalid(
                "prefix projection ends in an incomplete journal record",
            ));
        }
        journal.sync_data()?;
        let proof = PrefixProjection {
            scope: "saved-prefix-projection".into(),
            boundary: Completion {
                schema: 1,
                session_id: self.header.session_id.clone(),
                source: self.header.source,
                first_sample: self.first_sample(),
                through_sample,
            },
            journal_bytes,
        };
        write_marker(
            &self.dir,
            self.header.source.prefix_projected_path(&self.dir),
            &proof,
        )
    }

    /// Match an exact validated journal prefix already committed to the DB.
    /// Additional unprocessed audio or a torn journal suffix is not completion.
    pub fn is_prefix_projected(&self, through_sample: u64, journal_bytes: u64) -> io::Result<bool> {
        let Some(proof): Option<PrefixProjection> =
            read_marker(self.header.source.prefix_projected_path(&self.dir))?
        else {
            return Ok(false);
        };
        let b = &proof.boundary;
        let journal = std::fs::metadata(self.header.source.coverage_path(&self.dir))?;
        Ok(proof.scope == "saved-prefix-projection"
            && journal.is_file()
            && journal.len() >= journal_bytes
            && b.schema == 1
            && b.session_id == self.header.session_id
            && b.source == self.header.source
            && b.first_sample == self.first_sample()
            && b.through_sample == through_sample
            && through_sample >= self.first_sample()
            && through_sample <= self.available_end()?
            && proof.journal_bytes == journal_bytes)
    }

    /// Whether inference and the current journal were durably projected.
    /// Retirement separately acquires both canonical and journal owner leases.
    pub fn is_projected(&self) -> io::Result<bool> {
        if !self.is_complete()? {
            return Ok(false);
        }
        let Some(proof): Option<ProjectionProof> =
            read_marker(self.header.source.projected_path(&self.dir))?
        else {
            return Ok(false);
        };
        if proof.boundary.schema != 1 || !self.matches_boundary(&proof.boundary)? {
            return Ok(false);
        }
        let journal = File::open(self.header.source.coverage_path(&self.dir))?;
        Ok(journal.metadata()?.is_file() && journal.metadata()?.len() == proof.journal_bytes)
    }

    /// Whether a bounded, valid proof matches this source's current saved end.
    /// This is a status snapshot; retirement also acquires exclusive ownership.
    pub fn is_complete(&self) -> io::Result<bool> {
        self.sealed_end()?;
        match self.read_boundary(self.header.source.completion_path(&self.dir))? {
            Some(boundary) => self.matches_boundary(&boundary),
            None => Ok(false),
        }
    }

    fn read_boundary(&self, path: PathBuf) -> io::Result<Option<Completion>> {
        let Some(boundary): Option<Completion> = read_marker(path)? else {
            return Ok(None);
        };
        if boundary.schema != 1 {
            return Err(invalid("unsupported replay boundary proof"));
        }
        Ok(Some(boundary))
    }

    fn matches_boundary(&self, boundary: &Completion) -> io::Result<bool> {
        Ok(self.aligned()?
            && boundary.session_id == self.header.session_id
            && boundary.source == self.header.source
            && boundary.first_sample == self.first_sample()
            && boundary.through_sample == self.available_end()?)
    }

    fn aligned(&self) -> io::Result<bool> {
        let length = self
            .file
            .metadata()?
            .len()
            .checked_sub(self.audio_offset)
            .ok_or_else(|| invalid("replay header truncated"))?;
        Ok(length % 2 == 0)
    }

    /// Make the prefix ending at `through_sample` durable before journaling a
    /// transcript checkpoint. The pump uses unbuffered writes, so syncing this
    /// independent handle flushes the same file's already-written prefix.
    ///
    /// This does not certify provider coverage: the controller must separately
    /// establish that inference completed, and fence the provider epoch.
    pub fn sync_through(&mut self, through_sample: u64) -> io::Result<()> {
        if through_sample < self.first_sample() || through_sample > self.available_end()? {
            return Err(invalid("checkpoint is outside saved replay audio"));
        }
        self.file.sync_data()
    }

    fn available_end(&self) -> io::Result<u64> {
        let length = self
            .file
            .metadata()?
            .len()
            .checked_sub(self.audio_offset)
            .ok_or_else(|| invalid("replay header truncated"))?;
        self.first_sample()
            .checked_add(length / 2)
            .ok_or_else(|| invalid("replay sample clock overflow"))
    }

    /// Read up to `maximum` samples without buffering the rest of an outage.
    ///
    /// Reading exactly at the current end returns empty; a cursor before the
    /// anchor or after the end is an error. A trailing half-sample from an
    /// interrupted write is excluded. This is an availability observation,
    /// not proof that these bytes were synced to stable storage.
    pub fn read_from(&mut self, absolute_sample: u64, maximum: usize) -> io::Result<Vec<i16>> {
        if maximum == 0 || maximum > MAX_READ_SAMPLES {
            return Err(invalid("replay reads must contain 1..=16000 samples"));
        }
        let relative = absolute_sample
            .checked_sub(self.header.first_sample)
            .ok_or_else(|| invalid("replay cursor precedes source anchor"))?;
        let samples = self.available_end()? - self.first_sample();
        let remaining = samples
            .checked_sub(relative)
            .ok_or_else(|| invalid("replay cursor exceeds available audio"))?;
        let count = remaining.min(maximum as u64) as usize;
        let offset = relative
            .checked_mul(2)
            .and_then(|n| n.checked_add(self.audio_offset))
            .ok_or_else(|| invalid("replay byte offset overflow"))?;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; count * 2];
        self.file.read_exact(&mut bytes)?;
        Ok(bytes
            .chunks_exact(2)
            .map(|v| i16::from_le_bytes([v[0], v[1]]))
            .collect())
    }
}

#[cfg(test)]
mod owner_release_tests {
    use super::*;

    #[test]
    fn releasing_an_owner_unlocks_even_if_a_spawned_child_still_holds_a_duplicate_descriptor() {
        let dir = std::env::temp_dir().join(format!(
            "fotw-replay-inherited-owner-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let writer = ReplayWriter::create(&dir, "fixture", ReplaySource::System, 0).unwrap();
        // A concurrent process spawn can inherit the open file description
        // until exec closes CLOEXEC descriptors. dup models that deterministically.
        let inherited = writer._owner.0.try_clone().unwrap();
        drop(writer);
        let resumed = ReplayWriter::resume(&dir, "fixture", ReplaySource::System);
        assert!(
            resumed.is_ok(),
            "the departed owner must release the lock: {:?}",
            resumed.as_ref().err()
        );
        drop(resumed);
        drop(inherited);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
