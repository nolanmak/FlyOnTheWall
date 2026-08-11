//! The write-ahead session log.
//!
//! **Audio-to-disk is the crash invariant. The transcript is derived and
//! recomputable.** This module is the reason a meeting is never lost to a
//! crash, a network stall, or a provider outage (docs/REQUIREMENTS.md 5.4).
//!
//! ```text
//! sessions/<ulid>/
//!   system.pcm      headerless raw i16 @ the session rate
//!   mic.pcm
//!   manifest.json   rates, channels, epochs, app version, gap markers;
//!                   `ended_at_ms` is ABSENT until a clean finalize
//!   stt.jsonl       append-only, one object per finalized STT result
//!   notes.json      the user's typed notes
//! ```
//!
//! # Why headerless PCM
//!
//! A RIFF/WAVE header carries the data length, so it has to be rewritten when
//! the file closes. A hard kill leaves it wrong, and a reader that trusts it
//! either truncates the recording or refuses it outright. Raw samples plus a
//! manifest removes that failure mode: the file length *is* the length.
//!
//! # Why `ended_at_ms` is absent rather than false
//!
//! Its absence is the recovery signal. A session directory that has no
//! `ended_at_ms` was not closed cleanly, and the app offers to recover it.
//! Encoding that as a boolean would mean writing `false` at creation and
//! `true` at the end — two writes where one will do, and the failure mode of
//! the first one not landing is a session that can never be recovered.
//!
//! # Durability
//!
//! Appends go through a `BufWriter` and are flushed plus `sync_data`'d on a
//! cadence, so a hard kill costs at most one interval. Nothing here relies on
//! `Drop`, a panic hook, or a signal handler running: those are best-effort
//! improvements, not the mechanism.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How often buffered data is forced to disk.
///
/// The spec's acceptance criterion is that a kill loses at most five seconds,
/// so the cadence has to be strictly under that with room for scheduling.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(2);

/// Sample width on disk. i16 rather than f32 halves the footprint and is the
/// format every STT provider wants anyway.
const BYTES_PER_SAMPLE: u64 = 2;

/// An interval of audio that was not captured.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Gap {
    /// Session-relative start.
    pub start_ms: u64,
    /// Session-relative end.
    pub end_ms: u64,
    /// Why the gap exists — a device rebuild, sleep/wake, a tap stall.
    pub reason: String,
}

impl Gap {
    /// How long the gap lasted.
    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// Everything needed to interpret the PCM files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Session id, also the directory name.
    pub id: String,
    /// Sample rate of both PCM files.
    pub sample_rate_hz: u32,
    /// Channels in each PCM file.
    pub channels: u16,
    /// Wall clock at session start, for display.
    pub started_at_ms: u64,
    /// Monotonic host clock at session start, for alignment.
    pub host_epoch_ns: u64,
    /// Version that wrote this, so a future reader can adapt.
    pub app_version: String,
    /// Schema version of this manifest.
    pub schema: u32,
    /// Absent until a clean finalize. Its absence is the recovery signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    /// Intervals that were not captured.
    #[serde(default)]
    pub gaps: Vec<Gap>,
}

/// One finalized STT result, as appended to `stt.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SttRecord {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Session-relative start.
    pub t0_ms: u64,
    /// Session-relative end.
    pub t1_ms: u64,
    /// The transcribed text.
    pub text: String,
    /// Byte offset into `system.pcm` this result corresponds to, so a partial
    /// transcript can be resumed from the audio rather than from scratch.
    pub audio_byte_offset: u64,
}

/// A live session being written to disk.
#[derive(Debug)]
pub struct SessionWal {
    dir: PathBuf,
    manifest: Manifest,
    system: BufWriter<File>,
    mic: BufWriter<File>,
    stt: BufWriter<File>,
    last_sync: Instant,
}

impl SessionWal {
    /// Create a new session directory under `root`.
    pub fn create(
        root: impl AsRef<Path>,
        sample_rate_hz: u32,
        channels: u16,
    ) -> std::io::Result<Self> {
        let id = session_id();
        let dir = root.as_ref().join(&id);
        std::fs::create_dir_all(&dir)?;

        let manifest = Manifest {
            id,
            sample_rate_hz,
            channels,
            started_at_ms: now_ms(),
            host_epoch_ns: 0,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            schema: 1,
            ended_at_ms: None,
            gaps: Vec::new(),
        };

        let wal = Self {
            system: BufWriter::new(create(&dir.join("system.pcm"))?),
            mic: BufWriter::new(create(&dir.join("mic.pcm"))?),
            stt: BufWriter::new(create(&dir.join("stt.jsonl"))?),
            manifest,
            dir,
            last_sync: Instant::now(),
        };
        // Write the manifest immediately: a session that crashes one second in
        // must still be identifiable as a session.
        wal.write_manifest()?;
        Ok(wal)
    }

    /// The session directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The manifest as it currently stands.
    #[must_use]
    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Append system-audio samples.
    pub fn write_system(&mut self, pcm: &[f32]) -> std::io::Result<()> {
        write_pcm(&mut self.system, pcm)?;
        self.maybe_sync()
    }

    /// Append microphone samples.
    pub fn write_mic(&mut self, pcm: &[f32]) -> std::io::Result<()> {
        write_pcm(&mut self.mic, pcm)?;
        self.maybe_sync()
    }

    /// Append a finalized STT result.
    pub fn append_stt(&mut self, record: &SttRecord) -> std::io::Result<()> {
        // One JSON object per line, appended and never rewritten, so a torn
        // final line costs exactly one record.
        serde_json::to_writer(&mut self.stt, record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.stt.write_all(b"\n")?;
        self.maybe_sync()
    }

    /// Record an interval that was not captured.
    ///
    /// Gaps are written through to the manifest immediately: the whole point
    /// of a gap marker is to survive the crash that may follow the event that
    /// caused it.
    pub fn mark_gap(
        &mut self,
        start_ms: u64,
        end_ms: u64,
        reason: impl Into<String>,
    ) -> std::io::Result<()> {
        self.manifest.gaps.push(Gap {
            start_ms,
            end_ms,
            reason: reason.into(),
        });
        self.write_manifest()
    }

    /// Force everything to disk.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.system.flush()?;
        self.system.get_ref().sync_data()?;
        self.mic.flush()?;
        self.mic.get_ref().sync_data()?;
        self.stt.flush()?;
        self.stt.get_ref().sync_data()?;
        self.last_sync = Instant::now();
        Ok(())
    }

    /// Close the session cleanly, stamping `ended_at_ms`.
    ///
    /// After this the session is no longer offered for recovery.
    pub fn finalize(mut self) -> std::io::Result<PathBuf> {
        self.flush()?;
        self.manifest.ended_at_ms = Some(now_ms());
        self.write_manifest()?;
        Ok(self.dir)
    }

    fn maybe_sync(&mut self) -> std::io::Result<()> {
        if self.last_sync.elapsed() >= SYNC_INTERVAL {
            self.flush()?;
        }
        Ok(())
    }

    fn write_manifest(&self) -> std::io::Result<()> {
        // Temp file plus rename: a manifest half-overwritten by a crash would
        // make an otherwise-recoverable session unreadable.
        let tmp = self.dir.join("manifest.json.tmp");
        let json = serde_json::to_vec_pretty(&self.manifest)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        {
            let mut f = create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        std::fs::rename(tmp, self.dir.join("manifest.json"))
    }
}

/// A session found on disk, with whatever survived.
#[derive(Debug)]
pub struct SessionState {
    /// Where it lives.
    pub dir: PathBuf,
    /// Its manifest.
    pub manifest: Manifest,
    /// Whole frames readable from `system.pcm`.
    pub system_frames: u64,
    /// Whole frames readable from `mic.pcm`.
    pub mic_frames: u64,
    /// Complete STT records. A torn trailing line is discarded.
    pub stt: Vec<SttRecord>,
}

impl SessionState {
    /// Read a session directory, tolerating a hard kill mid-write.
    pub fn read(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let manifest: Manifest = {
            let mut s = String::new();
            File::open(dir.join("manifest.json"))?.read_to_string(&mut s)?;
            serde_json::from_str(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        };

        let frame_bytes = BYTES_PER_SAMPLE * u64::from(manifest.channels.max(1));
        // Integer division deliberately discards a partial trailing frame: a
        // kill mid-write leaves one, and rejecting the file over it would
        // throw away the entire meeting.
        let frames = |name: &str| -> u64 {
            std::fs::metadata(dir.join(name))
                .map(|m| m.len() / frame_bytes)
                .unwrap_or(0)
        };

        Ok(Self {
            system_frames: frames("system.pcm"),
            mic_frames: frames("mic.pcm"),
            stt: read_jsonl(&dir.join("stt.jsonl")),
            manifest,
            dir,
        })
    }

    /// Whether this session was closed cleanly.
    #[must_use]
    pub const fn is_finalized(&self) -> bool {
        self.manifest.ended_at_ms.is_some()
    }

    /// Seconds of system audio that survived.
    #[must_use]
    pub fn system_seconds(&self) -> f64 {
        self.system_frames as f64 / f64::from(self.manifest.sample_rate_hz.max(1))
    }
}

/// Every session under `root` that was not finalized.
///
/// This is what surfaces the "Recover meeting from &lt;time&gt;" action.
pub fn recover(root: impl AsRef<Path>) -> std::io::Result<Vec<SessionState>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        // A directory we cannot parse is skipped rather than failing the whole
        // scan: one corrupt session must not hide every other one.
        if let Ok(state) = SessionState::read(entry.path())
            && !state.is_finalized()
        {
            out.push(state);
        }
    }
    out.sort_by_key(|s| s.manifest.started_at_ms);
    Ok(out)
}

fn read_jsonl(path: &Path) -> Vec<SttRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        // A torn final line simply fails to parse and is dropped. Every
        // complete record before it survives.
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn write_pcm(w: &mut BufWriter<File>, pcm: &[f32]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        let v = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
        buf.extend_from_slice(&v.to_le_bytes());
    }
    w.write_all(&buf)
}

fn create(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A lexicographically sortable, collision-resistant session id.
fn session_id() -> String {
    let ms = now_ms();
    // Enough entropy to avoid a collision between two sessions created in the
    // same millisecond, without taking a uuid dependency in this crate.
    let n = std::time::Instant::now().elapsed().subsec_nanos() as u64
        ^ std::process::id() as u64
        ^ (&ms as *const u64 as u64);
    format!("{ms:013}-{:08x}", n & 0xffff_ffff)
}
