//! Admit stopped WALs using the saved clock, verified retained audio and canonical input.
//!
//! Raw PCM has no persisted reconstruction map for the live filter's state and
//! route boundaries. It is validation evidence, never replacement STT input.
//! Unknown clocks, extra raw tails, missing canonical input and live ownership
//! retain all files for the separate raw-prefix recovery path.
use crate::failover::coverage::CoverageJournal;
use fotw_pipeline::{
    capture_clock::{CaptureOwner, CaptureSnapshot},
    replay::{ReplayReader, ReplaySource, ReplayWriter},
};
use fotw_stt::Source;
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

pub(crate) struct StoppedCapture {
    pub dir: PathBuf,
    pub session: String,
    pub started: u64,
    pub ended: u64,
    // Hold every lease until the database projection has returned.
    _audio: Vec<ReplayWriter>,
    _journals: Vec<CoverageJournal>,
    // Struct fields drop in order: release the outer lease last.
    _capture: CaptureOwner,
}
struct Boundary {
    source: Source,
    replay: ReplaySource,
    end: u64,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn ordinary(path: &Path) -> io::Result<()> {
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid("capture evidence is not an ordinary file"));
    }
    Ok(())
}
fn optional_ordinary(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        _ => Err(invalid("capture sidecar is not an ordinary file")),
    }
}
fn present(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn snapshot(dir: &Path) -> io::Result<(CaptureOwner, CaptureSnapshot)> {
    ordinary(&dir.join("manifest.json"))?;
    if std::fs::metadata(dir.join("manifest.json"))?.len() > 1024 * 1024 {
        return Err(invalid("capture manifest exceeds size bound"));
    }
    let owner = CaptureOwner::recover(dir)?;
    let proof = owner.snapshot()?;
    Ok((owner, proof))
}
fn boundaries(dir: &Path, proof: &CaptureSnapshot) -> io::Result<Vec<Boundary>> {
    let clock_end = proof
        .checkpoint
        .elapsed_ms
        .checked_mul(16)
        .ok_or_else(|| invalid("capture clock overflow"))?;
    let mut out = Vec::new();
    for (source, replay, name, bytes, format) in [
        (
            Source::System,
            ReplaySource::System,
            "system",
            proof.checkpoint.system_bytes,
            Some(proof.checkpoint.system_format),
        ),
        (
            Source::Mic,
            ReplaySource::Mic,
            "mic",
            proof.checkpoint.mic_bytes,
            proof.checkpoint.mic_format,
        ),
    ] {
        let journal = dir.join(format!("{name}.stt-coverage.jsonl"));
        // A recorded leg need not have been configured for transcription.
        if !present(&journal)? {
            if present(&dir.join(format!("{name}.stt16")))? {
                return Err(invalid("canonical input has no coverage identity"));
            }
            continue;
        }
        ordinary(&journal)?;
        ordinary(&dir.join(format!("{name}.stt16")))?;
        for suffix in [
            "stt16.lock",
            "stt-coverage.jsonl.lock",
            "stt-sealed.json",
            "stt-complete.json",
            "stt-projected.json",
        ] {
            optional_ordinary(&dir.join(format!("{name}.{suffix}")))?;
        }
        let raw_samples = match format {
            Some(f)
                if (4000..=768000).contains(&f.sample_rate_hz)
                    && (1..=32).contains(&f.channels) =>
            {
                u64::try_from(
                    u128::from(bytes / 2 / u64::from(f.channels)) * 16000
                        / u128::from(f.sample_rate_hz),
                )
                .map_err(|_| invalid("capture sample clock overflow"))?
            }
            None if bytes == 0 => 0,
            _ => return Err(invalid("capture has no valid source format")),
        };
        let reader = ReplayReader::open(dir, &proof.checkpoint.session_id, replay)?;
        let end = reader
            .first_sample()
            .checked_add(raw_samples)
            .ok_or_else(|| invalid("capture sample clock overflow"))?;
        if end > clock_end || reader.saved_end()? != end {
            return Err(invalid("canonical/raw duration reconciliation is required"));
        }
        reader.sealed_end()?;
        let saved = CoverageJournal::snapshot(dir, &proof.checkpoint.session_id, source)?;
        let through = saved
            .commits
            .last()
            .map_or(reader.first_sample(), |c| c.through_sample);
        if through > end || (through < end && saved.fallback.is_none()) {
            return Err(invalid("pending input has no saved local plan"));
        }
        out.push(Boundary {
            source,
            replay,
            end,
        });
    }
    Ok(out)
}

/// Freeze existing canonical input under all leases before projecting a stopped WAL.
/// This never reconstructs PCM, stamps a clean manifest, or guesses a capture end.
pub(crate) fn prepare(dir: &Path, stop: &AtomicBool) -> io::Result<Option<StoppedCapture>> {
    let (owner, proof) = snapshot(dir)?;
    let Some((started, ended)) = proof.frozen_capture_clock() else {
        return Err(invalid("interrupted capture has no proven end"));
    };
    let bounds = boundaries(dir, &proof)?;
    if bounds.is_empty() {
        return Err(invalid("interrupted capture has no canonical source plan"));
    }
    let mut audio = Vec::new();
    let mut journals = Vec::new();
    // Acquire ALL source leases and validate before changing any seal or SQL row.
    for boundary in &bounds {
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let writer = ReplayWriter::resume(dir, &proof.checkpoint.session_id, boundary.replay)?;
        let journal = CoverageJournal::open(dir, &proof.checkpoint.session_id, boundary.source)?;
        if writer.end_sample() != boundary.end || journal.committed_through() > boundary.end {
            return Err(invalid("source changed during recovery admission"));
        }
        audio.push(writer);
        journals.push(journal);
    }
    for writer in &mut audio {
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        writer.seal()?;
    }
    Ok(Some(StoppedCapture {
        dir: dir.to_owned(),
        session: proof.checkpoint.session_id,
        started,
        ended,
        _capture: owner,
        _audio: audio,
        _journals: journals,
    }))
}

/// Retain capture ownership throughout local inference for an imported unfinished WAL.
pub(crate) fn own_stopped(dir: &Path, started: u64, ended: u64) -> io::Result<CaptureOwner> {
    let (owner, proof) = snapshot(dir)?;
    verify_stopped_snapshot(dir, &proof, started, ended)?;
    Ok(owner)
}

/// Revalidate a capture whose lease is already retained outside the async runtime.
pub(crate) fn verify_stopped(
    dir: &Path,
    owner: &CaptureOwner,
    started: u64,
    ended: u64,
) -> io::Result<()> {
    verify_stopped_snapshot(dir, &owner.snapshot()?, started, ended)
}
fn verify_stopped_snapshot(
    dir: &Path,
    proof: &CaptureSnapshot,
    started: u64,
    ended: u64,
) -> io::Result<()> {
    if proof.frozen_capture_clock() != Some((started, ended)) {
        return Err(invalid("saved capture clock has no matching Stop proof"));
    }
    if dir.file_name().and_then(|s| s.to_str()) != Some(proof.checkpoint.session_id.as_str()) {
        return Err(invalid("capture lease belongs to another session"));
    }
    for bound in boundaries(dir, proof)? {
        if ReplayReader::open(dir, &proof.checkpoint.session_id, bound.replay)?.sealed_end()?
            != Some(bound.end)
        {
            return Err(invalid("interrupted input has not been frozen"));
        }
    }
    Ok(())
}
