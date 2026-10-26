//! Processing provenance for canonical speech input, independent of ASR provider.
use fotw_pipeline::{
    aec::OutputKind,
    replay::{ReplayReader, ReplaySource},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

/// Immutable identifiers of a preloaded audio processor, never a credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    /// Audio processing backend identifier.
    pub backend: String,
    /// Exact native runtime version.
    pub version: String,
    /// Explicit parameter/profile revision.
    pub profile: String,
}
/// Bounded aggregate; validation does not retain a recording's interval list.
#[derive(Debug)]
pub struct Snapshot {
    /// Immutable backend configuration.
    pub descriptor: Descriptor,
    /// First absolute canonical sample.
    pub first_sample: u64,
    /// End of fully validated processing/audio coverage.
    pub end_sample: u64,
    /// Number of validated intervals.
    pub intervals: u64,
    /// Samples emitted by the processor.
    pub cleaned_samples: u64,
    /// Samples passed through without processing.
    pub raw_samples: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u32,
    session: String,
    source: ReplaySource,
    first_sample: u64,
    descriptor: Descriptor,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Interval {
    epoch: u64,
    start: u64,
    end: u64,
    kind: OutputKind,
    pcm_sha256: String,
}
const MAX_RECORD: usize = 4096;
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn name(source: ReplaySource) -> &'static str {
    match source {
        ReplaySource::System => "system",
        ReplaySource::Mic => "mic",
    }
}
fn path(dir: &Path, source: ReplaySource) -> PathBuf {
    dir.join(format!("{}.stt-processing.jsonl", name(source)))
}
impl Descriptor {
    pub(crate) fn fingerprint(&self) -> io::Result<String> {
        self.validate()?;
        let mut hash = Sha256::new();
        hash.update(b"fotw-processing-v1\0");
        hash.update(serde_json::to_vec(self)?);
        Ok(hex(hash.finalize()))
    }
    pub(crate) fn validate(&self) -> io::Result<()> {
        if [&self.backend, &self.version, &self.profile]
            .iter()
            .any(|s| {
                s.is_empty()
                    || s.len() > 96
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
            })
        {
            return Err(invalid("invalid audio processor identifier"));
        }
        Ok(())
    }
}
/// Presence includes malformed markers; neither raw planning nor reconstruction
/// may treat an invalid or symlinked processing declaration as ordinary input.
pub(crate) fn present(dir: &Path, source: ReplaySource) -> io::Result<bool> {
    match std::fs::symlink_metadata(path(dir, source)) {
        Ok(m) if m.file_type().is_file() => Ok(true),
        Ok(_) => Err(invalid("processing evidence is not an ordinary file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn digest(pcm: &[i16]) -> String {
    let mut hash = Sha256::new();
    for sample in pcm {
        hash.update(sample.to_le_bytes());
    }
    hex(hash.finalize())
}
fn hex(bytes: impl IntoIterator<Item = u8>) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(64);
    for byte in bytes {
        write!(&mut text, "{byte:02x}").unwrap();
    }
    text
}
fn line<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RECORD {
        return Err(invalid("processing record exceeds bound"));
    }
    Ok(bytes)
}
pub(crate) struct Journal {
    file: File,
    end: u64,
    epoch: Option<u64>,
    failed: bool,
}
impl Journal {
    pub(crate) fn check(&self, epoch: u64, start: u64, count: usize) -> io::Result<()> {
        if self.failed
            || start != self.end
            || !(1..=16000).contains(&count)
            || self.epoch.is_some_and(|old| epoch < old)
            || start.checked_add(count as u64).is_none()
        {
            return Err(invalid("invalid processing interval"));
        }
        Ok(())
    }
    pub(crate) fn create(
        dir: &Path,
        session: &str,
        source: ReplaySource,
        first_sample: u64,
        descriptor: Descriptor,
    ) -> io::Result<Self> {
        descriptor.validate()?;
        let audio = ReplayReader::open(dir, session, source)?;
        let fingerprint = descriptor.fingerprint()?;
        if audio.first_sample() != first_sample
            || audio.processing_id() != Some(fingerprint.as_str())
        {
            return Err(invalid(
                "processing declaration is not bound to the canonical header",
            ));
        }
        if session.is_empty() || session.len() > 512 {
            return Err(invalid("invalid processing session identity"));
        }
        match std::fs::symlink_metadata(dir.join(format!("{}.stt-raw-plan.json", name(source)))) {
            Ok(_) => {
                return Err(invalid(
                    "processed input conflicts with a raw reconstruction recipe",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let bytes = line(&Header {
            version: 1,
            session: session.into(),
            source,
            first_sample,
            descriptor,
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path(dir, source))?;
        // An incomplete declaration is retained on failure so it cannot be
        // mistaken for an unprocessed source by a later recovery attempt.
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(Self {
            file,
            end: first_sample,
            epoch: None,
            failed: false,
        })
    }
    pub(crate) fn record(
        &mut self,
        epoch: u64,
        start: u64,
        kind: OutputKind,
        pcm: &[i16],
    ) -> io::Result<()> {
        self.check(epoch, start, pcm.len())?;
        let end = start
            .checked_add(pcm.len() as u64)
            .ok_or_else(|| invalid("processing clock overflow"))?;
        let bytes = line(&Interval {
            epoch,
            start,
            end,
            kind,
            pcm_sha256: digest(pcm),
        })?;
        if let Err(e) = self.file.write_all(&bytes) {
            self.failed = true;
            return Err(e);
        }
        self.end = end;
        self.epoch = Some(epoch);
        Ok(())
    }
    pub(crate) fn sync(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(invalid("processing journal is fenced after a failed write"));
        }
        if let Err(e) = self.file.sync_all() {
            self.failed = true;
            return Err(e);
        }
        Ok(())
    }
}
/// Validate optional provenance against exact retained canonical PCM, using
/// bounded record reads. Absence denotes the existing unannotated input format.
pub fn validate(
    dir: &Path,
    session: &str,
    source: ReplaySource,
    audio: &mut ReplayReader,
) -> io::Result<Option<Snapshot>> {
    if !audio.has_identity(session, source) {
        return Err(invalid("processing reader identity differs"));
    }
    if !present(dir, source)? {
        if audio.processing_id().is_some() {
            return Err(invalid(
                "processed canonical audio is missing its processing evidence",
            ));
        }
        return Ok(None);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path(dir, source))?;
    if !file.metadata()?.is_file() {
        return Err(invalid("processing evidence is not a regular file"));
    }
    let mut reader = BufReader::new(file);
    let first = record(&mut reader)?.ok_or_else(|| invalid("missing processing declaration"))?;
    let header: Header = serde_json::from_slice(&first)?;
    header.descriptor.validate()?;
    if header.version != 1
        || header.session != session
        || header.source != source
        || header.first_sample != audio.first_sample()
        || audio.processing_id() != Some(header.descriptor.fingerprint()?.as_str())
    {
        return Err(invalid("processing declaration identity differs"));
    }
    let mut snapshot = Snapshot {
        descriptor: header.descriptor,
        first_sample: header.first_sample,
        end_sample: header.first_sample,
        intervals: 0,
        cleaned_samples: 0,
        raw_samples: 0,
    };
    let mut epoch = None;
    while let Some(bytes) = record(&mut reader)? {
        let interval: Interval = serde_json::from_slice(&bytes)?;
        let count = interval
            .end
            .checked_sub(interval.start)
            .filter(|n| (1..=16000).contains(n))
            .ok_or_else(|| invalid("invalid processing interval extent"))?;
        if interval.start != snapshot.end_sample || epoch.is_some_and(|old| interval.epoch < old) {
            return Err(invalid("discontinuous processing coverage"));
        }
        let pcm = audio.read_from(interval.start, count as usize)?;
        if pcm.len() != count as usize || digest(&pcm) != interval.pcm_sha256 {
            return Err(invalid(
                "processing evidence differs from retained canonical PCM",
            ));
        }
        snapshot.end_sample = interval.end;
        snapshot.intervals += 1;
        epoch = Some(interval.epoch);
        match interval.kind {
            OutputKind::Cleaned => snapshot.cleaned_samples += count,
            OutputKind::Raw(_) => snapshot.raw_samples += count,
        }
    }
    if snapshot.end_sample != audio.saved_end()? {
        return Err(invalid("canonical PCM has no complete processing coverage"));
    }
    Ok(Some(snapshot))
}
fn record(reader: &mut BufReader<File>) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    Read::take(&mut *reader, (MAX_RECORD + 1) as u64).read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_RECORD || bytes.last() != Some(&b'\n') {
        return Err(invalid("torn or oversized processing record"));
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_pipeline::{aec::BypassReason, replay::ReplayWriter};
    fn descriptor() -> Descriptor {
        Descriptor {
            backend: "fixture-aec".into(),
            version: "1.0".into(),
            profile: "profile-v1".into(),
        }
    }
    #[test]
    fn malformed_oversized_or_rebound_declarations_never_rewrite_audio() {
        let dir = tempfile::tempdir().unwrap();
        let _audio = ReplayWriter::create_with_processing(
            dir.path(),
            "fixture",
            ReplaySource::Mic,
            0,
            &descriptor().fingerprint().unwrap(),
        )
        .unwrap();
        let _journal =
            Journal::create(dir.path(), "fixture", ReplaySource::Mic, 0, descriptor()).unwrap();
        let path = path(dir.path(), ReplaySource::Mic);
        let valid = std::fs::read(&path).unwrap();
        let mut rebound: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        rebound["descriptor"]["profile"] = "different-profile".into();
        let mut future: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        future["version"] = 2.into();
        let mut unknown: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        unknown["unknown_field"] = true.into();
        let canonical = std::fs::read(dir.path().join("mic.stt16")).unwrap();
        for bytes in [
            valid[..valid.len() - 1].to_vec(),
            vec![b'x'; MAX_RECORD + 10000],
            format!("{rebound}\n").into_bytes(),
            format!("{future}\n").into_bytes(),
            format!("{unknown}\n").into_bytes(),
        ] {
            std::fs::write(&path, &bytes).unwrap();
            let mut reader = ReplayReader::open(dir.path(), "fixture", ReplaySource::Mic).unwrap();
            assert!(validate(dir.path(), "fixture", ReplaySource::Mic, &mut reader).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(
                std::fs::read(dir.path().join("mic.stt16")).unwrap(),
                canonical
            );
        }
    }
    #[test]
    fn processed_and_raw_intervals_bind_exact_pcm_identity_and_source_clock() {
        let dir = tempfile::tempdir().unwrap();
        let mut audio = ReplayWriter::create_with_processing(
            dir.path(),
            "fixture",
            ReplaySource::Mic,
            160,
            &descriptor().fingerprint().unwrap(),
        )
        .unwrap();
        let mut journal =
            Journal::create(dir.path(), "fixture", ReplaySource::Mic, 160, descriptor()).unwrap();
        journal
            .record(7, 160, OutputKind::Cleaned, &[11, 22, 33])
            .unwrap();
        audio.append(&[11, 22, 33]).unwrap();
        journal
            .record(
                8,
                163,
                OutputKind::Raw(BypassReason::MissingReference),
                &[44, 55],
            )
            .unwrap();
        audio.append(&[44, 55]).unwrap();
        journal.sync().unwrap();
        audio.sync().unwrap();
        let mut reader = ReplayReader::open(dir.path(), "fixture", ReplaySource::Mic).unwrap();
        let saved = validate(dir.path(), "fixture", ReplaySource::Mic, &mut reader)
            .unwrap()
            .unwrap();
        assert_eq!(saved.descriptor, descriptor());
        assert_eq!(
            (
                saved.first_sample,
                saved.end_sample,
                saved.intervals,
                saved.cleaned_samples,
                saved.raw_samples
            ),
            (160, 165, 2, 3, 2)
        );
        assert!(validate(dir.path(), "other", ReplaySource::Mic, &mut reader).is_err());
    }
    #[test]
    fn invalid_or_partial_metadata_cannot_certify_audio_and_never_rewrites_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut audio = ReplayWriter::create_with_processing(
            dir.path(),
            "fixture",
            ReplaySource::Mic,
            0,
            &descriptor().fingerprint().unwrap(),
        )
        .unwrap();
        let mut journal =
            Journal::create(dir.path(), "fixture", ReplaySource::Mic, 0, descriptor()).unwrap();
        journal
            .record(7, 0, OutputKind::Cleaned, &[11, 22, 33])
            .unwrap();
        let path = dir.path().join("mic.stt-processing.jsonl");
        let before = std::fs::read(&path).unwrap();
        for (epoch, start, pcm) in [
            (6, 3, vec![22]),
            (7, 4, vec![22]),
            (7, 3, vec![]),
            (7, 3, vec![22; 16001]),
        ] {
            assert!(
                journal
                    .record(epoch, start, OutputKind::Cleaned, &pcm)
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        let mut reader = ReplayReader::open(dir.path(), "fixture", ReplaySource::Mic).unwrap();
        assert!(
            validate(dir.path(), "fixture", ReplaySource::Mic, &mut reader).is_err(),
            "metadata ahead of durable PCM is unproven"
        );
        audio.append(&[11, 22, 34]).unwrap();
        assert!(
            validate(dir.path(), "fixture", ReplaySource::Mic, &mut reader).is_err(),
            "same-length different PCM cannot pass the hash"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
