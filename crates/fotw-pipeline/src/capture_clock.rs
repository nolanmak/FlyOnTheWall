//! Durable capture-clock evidence, independent of transcription and native Stop.
use crate::wal::TrackFormat;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

/// One fsynced raw-audio prefix on the original recording clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureCheckpoint {
    /// File schema, currently one.
    pub schema: u32,
    /// Original directory/manifest identity.
    pub session_id: String,
    /// Original declared recording start; never recovery time.
    pub started_at_ms: u64,
    /// Observed offset, frozen by the first Stop request.
    pub elapsed_ms: u64,
    /// A Stop request froze the clock. This does not prove native Stop/cleanup.
    pub stop_requested: bool,
    /// Durable system PCM16 bytes at this checkpoint.
    pub system_bytes: u64,
    /// Durable microphone PCM16 bytes at this checkpoint.
    pub mic_bytes: u64,
    /// Exact archived system format.
    pub system_format: TrackFormat,
    /// Exact archived microphone format, if present.
    pub mic_format: Option<TrackFormat>,
}
/// A checkpoint plus any retained audio not covered by its clock evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSnapshot {
    /// The last complete checkpoint.
    pub checkpoint: CaptureCheckpoint,
    /// System bytes beyond the checkpoint; never silently discarded.
    pub system_tail_bytes: u64,
    /// Microphone bytes beyond the checkpoint; never silently discarded.
    pub mic_tail_bytes: u64,
}
impl CaptureSnapshot {
    /// Exact Stop-request clock only when all current raw bytes were checkpointed.
    pub fn frozen_capture_clock(&self) -> Option<(u64, u64)> {
        if !self.checkpoint.stop_requested
            || self.system_tail_bytes != 0
            || self.mic_tail_bytes != 0
        {
            return None;
        }
        Some((
            self.checkpoint.started_at_ms,
            self.checkpoint
                .started_at_ms
                .checked_add(self.checkpoint.elapsed_ms)?,
        ))
    }
}
/// Exclusive recording/recovery ownership. A timeout cannot revoke this lease.
#[derive(Debug)]
pub struct CaptureOwner {
    file: File,
    dir: PathBuf,
}
impl Drop for CaptureOwner {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn ordinary(path: &Path) -> io::Result<()> {
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid("capture evidence must be an ordinary file"));
    }
    Ok(())
}
impl CaptureOwner {
    pub(crate) fn create(dir: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("capture-owner.lock"))?;
        file.try_lock().map_err(io::Error::from)?;
        Ok(Self {
            file,
            dir: dir.to_owned(),
        })
    }
    /// Acquire an existing writer's lease; never manufacture proof for legacy WALs.
    pub fn recover(dir: &Path) -> io::Result<Self> {
        if !std::fs::symlink_metadata(dir)?.file_type().is_dir() {
            return Err(invalid("capture directory is not an ordinary directory"));
        }
        let path = dir.join("capture-owner.lock");
        ordinary(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock().map_err(io::Error::from)?;
        Ok(Self {
            file,
            dir: dir.to_owned(),
        })
    }
    /// Read bounded clock evidence while owning the recording.
    pub fn snapshot(&self) -> io::Result<CaptureSnapshot> {
        let path = self.dir.join("capture-clock.json");
        ordinary(&path)?;
        let mut bytes = Vec::new();
        File::open(path)?.take(8193).read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            return Err(invalid("capture checkpoint exceeds size bound"));
        }
        let checkpoint: CaptureCheckpoint = serde_json::from_slice(&bytes)?;
        ordinary(&self.dir.join("manifest.json"))?;
        let manifest = crate::wal::read_manifest(&self.dir)?;
        if checkpoint.schema != 1
            || checkpoint.session_id != manifest.id
            || self.dir.file_name().and_then(|s| s.to_str()) != Some(checkpoint.session_id.as_str())
            || checkpoint.started_at_ms != manifest.started_at_ms
            || Some(checkpoint.system_format) != manifest.system_format
            || checkpoint.mic_format != manifest.mic_format
            || checkpoint
                .started_at_ms
                .checked_add(checkpoint.elapsed_ms)
                .is_none()
        {
            return Err(invalid(
                "capture checkpoint does not match original manifest",
            ));
        }
        let tail = |name: &str, count: u64, format: Option<TrackFormat>| -> io::Result<u64> {
            if let Some(format) = format {
                if format.sample_rate_hz == 0
                    || format.channels == 0
                    || !count.is_multiple_of(u64::from(format.channels) * 2)
                {
                    return Err(invalid("invalid checkpoint PCM format or alignment"));
                }
            } else if count != 0 {
                return Err(invalid("checkpoint audio has no declared format"));
            }
            let path = self.dir.join(name);
            ordinary(&path)?;
            std::fs::metadata(path)?
                .len()
                .checked_sub(count)
                .ok_or_else(|| invalid("checkpointed audio has been truncated"))
        };
        let system_tail_bytes = tail(
            "system.pcm",
            checkpoint.system_bytes,
            Some(checkpoint.system_format),
        )?;
        let mic_tail_bytes = tail("mic.pcm", checkpoint.mic_bytes, checkpoint.mic_format)?;
        Ok(CaptureSnapshot {
            checkpoint,
            system_tail_bytes,
            mic_tail_bytes,
        })
    }
    pub(crate) fn publish(&self, checkpoint: &CaptureCheckpoint) -> io::Result<()> {
        // A stale temporary file from an interrupted publisher is owned by this
        // same exclusive lease; it is never evidence on its own.
        let temp = self.dir.join("capture-clock.json.tmp");
        match std::fs::remove_file(&temp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(checkpoint)?)?;
        file.sync_all()?;
        std::fs::rename(temp, self.dir.join("capture-clock.json"))?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}
