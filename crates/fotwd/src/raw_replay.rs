//! Reconstruct missing canonical suffixes from a declared WAL processing recipe.
use fotw_pipeline::{
    capture_clock::CaptureSnapshot,
    replay::{ReconstructionOwner, ReplayReader, ReplaySource, ReplayWriter},
    resample::{ArchiveConverter, Downmixer},
    wal::TrackFormat,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const RECIPE: &str = "wal-i16-archive-converter-rubato5-v1";

#[cfg(test)]
mod compatibility {
    #[test]
    fn processed_header_without_its_sidecar_still_refuses_a_raw_recipe() {
        use fotw_pipeline::{
            replay::{ReplaySource, ReplayWriter},
            wal::TrackFormat,
        };
        let dir = tempfile::tempdir().unwrap();
        let _audio = ReplayWriter::create_with_processing(
            dir.path(),
            "fixture",
            ReplaySource::Mic,
            0,
            &"0".repeat(64),
        )
        .unwrap();
        assert!(
            super::save_plan(
                dir.path(),
                "fixture",
                ReplaySource::Mic,
                TrackFormat::new(16000, 1)
            )
            .is_err()
        );
        assert!(!dir.path().join("mic.stt-raw-plan.json").exists());
    }
    #[test]
    fn processed_declarations_cannot_acquire_a_raw_reconstruction_recipe() {
        use fotw_pipeline::{
            replay::{ReplaySource, ReplayWriter},
            wal::TrackFormat,
        };
        let dir = tempfile::tempdir().unwrap();
        let _audio = ReplayWriter::create(dir.path(), "fixture", ReplaySource::Mic, 0).unwrap();
        std::fs::write(
            dir.path().join("mic.stt-processing.jsonl"),
            b"incomplete declaration",
        )
        .unwrap();
        assert!(
            super::save_plan(
                dir.path(),
                "fixture",
                ReplaySource::Mic,
                TrackFormat::new(16000, 1)
            )
            .is_err(),
            "even invalid processing evidence must prevent an ordinary raw recipe"
        );
        assert!(!dir.path().join("mic.stt-raw-plan.json").exists());
    }
    #[test]
    fn processing_dependency_changes_require_an_explicit_recipe_compatibility_review() {
        let lock = include_str!("../../../Cargo.lock");
        let dependency = lock
            .split("[[package]]")
            .find(|entry| entry.contains("name = \"rubato\"\n"))
            .unwrap();
        assert!(
            dependency.contains("version = \"5.0.0\""),
            "Review persisted raw-processing recipe compatibility before changing its resampling dependency"
        );
    }
}

/// The live and recovery paths deliberately consume the same quantized WAL
/// samples, so recovering never substitutes a differently filtered prefix.
pub(crate) struct CanonicalInput(ArchiveConverter);
impl CanonicalInput {
    pub(crate) fn output_origin(&self) -> fotw_pipeline::resample::ConversionOrigin {
        self.0.output_origin()
    }
    pub fn new(format: TrackFormat) -> Result<Self, String> {
        ArchiveConverter::new(format.sample_rate_hz, format.channels, 16000, 1)
            .map(Self)
            .map_err(|e| e.to_string())
    }
    pub fn process(&mut self, pcm: &[f32]) -> Result<Vec<f32>, String> {
        self.process_raw(&Downmixer::to_i16(pcm))
    }
    fn process_raw(&mut self, pcm: &[i16]) -> Result<Vec<f32>, String> {
        let pcm: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32767.0).collect();
        self.0.process(&pcm).map_err(|e| e.to_string())
    }
    pub fn finish(&mut self) -> Result<Vec<f32>, String> {
        self.0.finish().map_err(|e| e.to_string())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    recipe: String,
    session_id: String,
    source: ReplaySource,
    format: TrackFormat,
    first_sample: u64,
}
fn name(source: ReplaySource) -> &'static str {
    match source {
        ReplaySource::System => "system",
        ReplaySource::Mic => "mic",
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn ordinary(path: &Path) -> io::Result<()> {
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid(
            "raw reconstruction evidence is not an ordinary file",
        ));
    }
    Ok(())
}
fn open(path: &Path) -> io::Result<File> {
    ordinary(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}
pub(crate) fn save_plan(
    dir: &Path,
    session: &str,
    source: ReplaySource,
    format: TrackFormat,
) -> io::Result<()> {
    if crate::failover::processing::present(dir, source)? {
        return Err(invalid(
            "processed input cannot use a raw reconstruction recipe",
        ));
    }
    let reader = ReplayReader::open(dir, session, source)?;
    if reader.processing_id().is_some() {
        return Err(invalid(
            "processed canonical input cannot acquire a raw reconstruction recipe",
        ));
    }
    let plan = Plan {
        recipe: RECIPE.into(),
        session_id: session.into(),
        source,
        format,
        first_sample: reader.first_sample(),
    };
    let path = dir.join(format!("{}.stt-raw-plan.json", name(source)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec(&plan)?)?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    result
}
struct Lease(File);
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
struct Candidate {
    writer: ReconstructionOwner,
    reader: ReplayReader,
    _directory: tempfile::TempDir,
}

/// Called only while the original capture lease is held. Preflight every
/// source and candidate before appending any missing samples.
pub(crate) fn repair(dir: &Path, proof: &CaptureSnapshot, stop: &AtomicBool) -> io::Result<()> {
    for source in [ReplaySource::System, ReplaySource::Mic] {
        if crate::failover::processing::present(dir, source)? {
            match std::fs::symlink_metadata(dir.join(format!("{}.stt-raw-plan.json", name(source))))
            {
                Ok(_) => {
                    return Err(invalid(
                        "processed input conflicts with a raw reconstruction recipe",
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    if !["system", "mic"]
        .iter()
        .any(|n| dir.join(format!("{n}.stt-raw-plan.json")).exists())
    {
        return Ok(());
    }
    let session = &proof.checkpoint.session_id;
    let mut writers = Vec::new();
    let mut leases = Vec::new();
    let mut retained_writers = Vec::new();
    for source in [ReplaySource::System, ReplaySource::Mic] {
        let prefix = name(source);
        for suffix in [
            "stt16",
            "stt-raw-plan.json",
            "stt-coverage.jsonl",
            "stt16.lock",
            "stt-coverage.jsonl.lock",
            "stt-sealed.json",
            "stt-complete.json",
            "stt-projected.json",
            "stt-prefix-projected.json",
        ] {
            let path = dir.join(format!("{prefix}.{suffix}"));
            match std::fs::symlink_metadata(&path) {
                Ok(_) => ordinary(&path)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        let path = dir.join(format!("{prefix}.stt-coverage.jsonl.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock().map_err(io::Error::from)?;
        leases.push(Lease(file));
        let plan_path = dir.join(format!("{prefix}.stt-raw-plan.json"));
        if plan_path.exists() {
            let mut bytes = Vec::new();
            open(&plan_path)?.take(4097).read_to_end(&mut bytes)?;
            if bytes.len() > 4096 {
                return Err(invalid("raw reconstruction plan exceeds bound"));
            }
            let plan: Plan = serde_json::from_slice(&bytes)?;
            let format = match source {
                ReplaySource::System => Some(proof.checkpoint.system_format),
                ReplaySource::Mic => proof.checkpoint.mic_format,
            };
            if plan.recipe != RECIPE
                || plan.session_id != *session
                || plan.source != source
                || Some(plan.format) != format
            {
                return Err(invalid("raw reconstruction processing identity differs"));
            }
            let writer = ReconstructionOwner::reserve(dir, session, source, plan.first_sample)?;
            writers.push((source, plan, writer));
        } else if dir.join(format!("{prefix}.stt16")).exists() {
            let writer = ReplayWriter::resume_prefix_validated(dir, session, source, |reader| {
                crate::failover::processing::validate(dir, session, source, reader).map(|_| ())
            })?;
            let stt = match source {
                ReplaySource::System => fotw_stt::Source::System,
                ReplaySource::Mic => fotw_stt::Source::Mic,
            };
            crate::failover::coverage::CoverageJournal::snapshot(dir, session, stt)?;
            retained_writers.push(writer);
        } else if dir.join(format!("{prefix}.stt-coverage.jsonl")).exists() {
            return Err(invalid("missing canonical input has no processing recipe"));
        }
    }
    let mut candidates = Vec::new();
    let mut retained_mapped = Vec::new();
    for (source, plan, mut writer) in writers {
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let prefix = name(source);
        let stt = match source {
            ReplaySource::System => fotw_stt::Source::System,
            ReplaySource::Mic => fotw_stt::Source::Mic,
        };
        let mut converter = CanonicalInput::new(plan.format).map_err(|e| invalid(&e))?;
        let raw_path = dir.join(format!("{prefix}.pcm"));
        let checkpoint_bytes = match source {
            ReplaySource::System => proof.checkpoint.system_bytes,
            ReplaySource::Mic => proof.checkpoint.mic_bytes,
        };
        let frame_bytes = u64::from(plan.format.channels) * 2;
        let checkpoint_end = plan
            .first_sample
            .checked_add(
                u64::try_from(
                    u128::from(checkpoint_bytes / frame_bytes) * 16000
                        / u128::from(plan.format.sample_rate_hz),
                )
                .map_err(|_| invalid("raw reconstruction clock overflow"))?,
            )
            .ok_or_else(|| invalid("raw reconstruction clock overflow"))?;
        if !raw_path.exists()
            && proof.frozen_capture_clock().is_some()
            && writer.retained_end()? == Some(checkpoint_end)
        {
            writer.validate_current_markers()?;
            crate::failover::coverage::CoverageJournal::snapshot(dir, session, stt)?;
            retained_mapped.push(writer);
            continue;
        }
        let mut raw = open(&raw_path)?;
        let length = raw.metadata()?.len();
        let length = length / frame_bytes * frame_bytes;
        let target = plan
            .first_sample
            .checked_add(
                u64::try_from(
                    u128::from(length / frame_bytes) * 16000
                        / u128::from(plan.format.sample_rate_hz),
                )
                .map_err(|_| invalid("raw reconstruction clock overflow"))?,
            )
            .ok_or_else(|| invalid("raw reconstruction clock overflow"))?;
        if writer.retained_end()?.is_some_and(|end| target <= end) {
            writer.validate_current_markers()?;
            crate::failover::coverage::CoverageJournal::snapshot(dir, session, stt)?;
            retained_mapped.push(writer);
            continue;
        }
        if proof.frozen_capture_clock().is_some()
            && target > proof.checkpoint.elapsed_ms.saturating_mul(16)
        {
            return Err(invalid("raw reconstruction exceeds original capture clock"));
        }
        let directory = tempfile::Builder::new()
            .prefix(".stt-reconstruction-")
            .tempdir_in(dir)?;
        let mut output =
            ReplayWriter::create(directory.path(), session, source, plan.first_sample)?;
        let mut remaining = length;
        let mut buffer = vec![0u8; 48000];
        while remaining > 0 {
            if stop.load(Ordering::Acquire) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = remaining.min(buffer.len() as u64) as usize;
            raw.read_exact(&mut buffer[..n])?;
            remaining -= n as u64;
            let pcm: Vec<i16> = buffer[..n]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            output.append(&Downmixer::to_i16(
                &converter.process_raw(&pcm).map_err(|e| invalid(&e))?,
            ))?;
        }
        output.append(&Downmixer::to_i16(
            &converter.finish().map_err(|e| invalid(&e))?,
        ))?;
        output.sync()?;
        if output.end_sample() != target
            || raw.metadata()?.len() / frame_bytes * frame_bytes != length
        {
            return Err(invalid("raw reconstruction input changed"));
        }
        drop(output);
        let audio = ReplayReader::open(directory.path(), session, source)?;
        let saved = crate::failover::coverage::CoverageJournal::snapshot_with_audio(
            dir, session, stt, audio,
        )?;
        if saved.fallback.is_none() {
            return Err(invalid("raw reconstruction has no saved local plan"));
        }
        let mut reader = ReplayReader::open(directory.path(), session, source)?;
        writer.validate_candidate(&mut reader, stop)?;
        candidates.push(Candidate {
            writer,
            reader,
            _directory: directory,
        });
    }
    for mut candidate in candidates {
        candidate.writer.restore(&mut candidate.reader, stop)?;
    }
    drop(leases);
    drop(retained_writers);
    drop(retained_mapped);
    Ok(())
}
