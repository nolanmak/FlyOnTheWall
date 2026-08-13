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
//!
//! # What happens to the PCM afterwards
//!
//! Nothing keeps it. Raw PCM is 32 kB/s per leg, so a 45-minute meeting is
//! 173 MB of it — the crash invariant is worth that *during* the meeting and
//! nothing like it afterwards. [`SessionWal::finalize_and_encode`] transcodes
//! both legs to the archival form §9.5 specifies — mono Opus in Ogg, 24 kbps
//! VBR — and only then unlinks the PCM. The order matters: the PCM is the
//! source of truth until something else provably is.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::opus::{OpusError, OpusOggWriter, SUPPORTED_RATES};
use crate::resample::{Downmixer, Resampler16k, TARGET_RATE};

/// How often buffered data is forced to disk.
///
/// The spec's acceptance criterion is that a kill loses at most five seconds,
/// so the cadence has to be strictly under that with room for scheduling.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(2);

/// Sample width on disk. i16 rather than f32 halves the footprint and is the
/// format every STT provider wants anyway.
const BYTES_PER_SAMPLE: u64 = 2;

/// The two legs of a session, as `(pcm file, opus file)`.
///
/// The names are the ones §9.2 uses for the archived artifacts, so a session
/// directory promoted into `media/` needs no renaming.
pub const LEGS: [(&str, &str); 2] = [("system.pcm", "system.opus"), ("mic.pcm", "mic.opus")];

/// How many PCM samples are read and encoded per iteration during a
/// post-finalize transcode. 64 Ki samples is 4 seconds at 16 kHz — large
/// enough that the syscall cost disappears, small enough that a 90-minute
/// meeting never needs more than a few hundred kilobytes of resident buffer.
const TRANSCODE_CHUNK_SAMPLES: usize = 65_536;

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
    /// The archival Opus tracks, present once the PCM has been transcoded.
    ///
    /// Absent while the session is live, which is what tells a reader that the
    /// `.pcm` files are still the source of truth. Once this is present the
    /// `.pcm` files may be gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded: Option<EncodedSession>,
    /// Which meeting this session became, and where its audio is going
    /// (§9.2). Written by [`crate::promote::claim`] once the meeting row
    /// exists.
    ///
    /// Its presence is what makes promotion resumable without a database: a
    /// session directory found after a crash still knows its own destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<crate::promote::Claim>,
    /// The tracks already published into `media/`, and their exact sizes.
    ///
    /// The commit point of [`crate::promote`]. Its presence means the media
    /// tree *should* hold this meeting; the sizes are what
    /// [`crate::promote::retire`] re-checks before it removes the only other
    /// copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted: Option<crate::promote::Promotion>,
}

/// One transcoded leg of a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodedTrack {
    /// File name within the session directory, e.g. `system.opus`.
    pub file: String,
    /// Size of the PCM it came from. Kept after the PCM is unlinked so the
    /// compression ratio remains reportable.
    pub pcm_bytes: u64,
    /// Size of the Ogg file, framing included.
    pub opus_bytes: u64,
    /// Duration of the encoded audio.
    pub duration_ms: u64,
    /// The rate the Opus stream was encoded at, which is the session rate
    /// unless it had to be resampled to something libopus accepts.
    pub sample_rate_hz: u32,
}

impl EncodedTrack {
    /// How much smaller the Opus file is than the PCM it replaced.
    #[must_use]
    pub fn compression_ratio(&self) -> f64 {
        if self.opus_bytes == 0 {
            return 0.0;
        }
        self.pcm_bytes as f64 / self.opus_bytes as f64
    }
}

/// Both legs of a session, transcoded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodedSession {
    /// The system-audio leg.
    pub system: EncodedTrack,
    /// The microphone leg.
    pub mic: EncodedTrack,
}

impl EncodedSession {
    /// Total Opus bytes.
    #[must_use]
    pub const fn opus_bytes(&self) -> u64 {
        self.system.opus_bytes + self.mic.opus_bytes
    }

    /// Total PCM bytes replaced.
    #[must_use]
    pub const fn pcm_bytes(&self) -> u64 {
        self.system.pcm_bytes + self.mic.pcm_bytes
    }

    /// Whole-session compression ratio.
    #[must_use]
    pub fn compression_ratio(&self) -> f64 {
        if self.opus_bytes() == 0 {
            return 0.0;
        }
        self.pcm_bytes() as f64 / self.opus_bytes() as f64
    }

    /// The two Opus files, resolved against the session directory.
    #[must_use]
    pub fn paths(&self, dir: impl AsRef<Path>) -> Vec<PathBuf> {
        let dir = dir.as_ref();
        vec![dir.join(&self.system.file), dir.join(&self.mic.file)]
    }
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
            // 2 adds `encoded`; 3 adds `claim` and `promoted`. Every one of
            // those fields is `#[serde(default)]`, so an older manifest still
            // reads; the bump exists to make the absence of a field
            // distinguishable from a writer that did not know about it.
            schema: 3,
            ended_at_ms: None,
            gaps: Vec::new(),
            encoded: None,
            claim: None,
            promoted: None,
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

    /// Close the session, transcode both legs to Opus, and unlink the PCM.
    ///
    /// This is the production finalize path. [`finalize`](Self::finalize) is
    /// kept separate because the two failure modes are not the same: a session
    /// that closes but fails to encode is intact and retryable, while one that
    /// never closed is a recovery case. Sequencing matters — the PCM is the
    /// crash invariant right up until the Opus exists, so it is unlinked
    /// *after* the encode returns and not before.
    pub fn finalize_and_encode(self) -> Result<(PathBuf, EncodedSession), OpusError> {
        let dir = self.finalize()?;
        let encoded = encode_session(&dir)?;
        discard_pcm(&dir)?;
        Ok((dir, encoded))
    }

    fn maybe_sync(&mut self) -> std::io::Result<()> {
        if self.last_sync.elapsed() >= SYNC_INTERVAL {
            self.flush()?;
        }
        Ok(())
    }

    fn write_manifest(&self) -> std::io::Result<()> {
        write_manifest_at(&self.dir, &self.manifest)
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
        let manifest = read_manifest(&dir)?;

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

/// Transcode a session's raw PCM legs into the archival Opus tracks of §9.5.
///
/// Works on any session directory, finalized or not, which is what lets a
/// recovered meeting be archived without first being replayed. The `.pcm`
/// files are left alone — see [`discard_pcm`] — and the manifest is rewritten
/// with an [`EncodedSession`] so a later reader knows which artifact is
/// authoritative.
///
/// The muxing is incremental (CAP-10): a kill part-way through leaves both
/// `.opus` files playable up to their last whole Ogg page, and the untouched
/// `.pcm` still holds everything, so the encode can simply be re-run.
pub fn encode_session(dir: impl AsRef<Path>) -> Result<EncodedSession, OpusError> {
    let dir = dir.as_ref();
    let mut manifest = read_manifest(dir)?;

    let system = encode_track(dir, LEGS[0].0, LEGS[0].1, &manifest)?;
    let mic = encode_track(dir, LEGS[1].0, LEGS[1].1, &manifest)?;
    let encoded = EncodedSession { system, mic };

    manifest.encoded = Some(encoded.clone());
    write_manifest_at(dir, &manifest)?;
    Ok(encoded)
}

/// Unlink a session's raw PCM, returning the bytes reclaimed.
///
/// Refuses if the manifest does not record an encode, because the PCM is the
/// only copy of the meeting until something else demonstrably is. That check
/// is the difference between "reclaim space" and "lose the meeting".
pub fn discard_pcm(dir: impl AsRef<Path>) -> std::io::Result<u64> {
    let dir = dir.as_ref();
    let manifest = read_manifest(dir)?;
    if manifest.encoded.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to unlink PCM: the manifest records no Opus encode",
        ));
    }
    let mut freed = 0u64;
    for (pcm, _) in LEGS {
        let p = dir.join(pcm);
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&p) {
            Ok(()) => freed += size,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(freed)
}

/// Read a session's manifest.
///
/// `pub(crate)` rather than private because [`crate::promote`] is the other
/// half of this file's life cycle and has to read and rewrite the same
/// document; duplicating the reader there would be two definitions of what a
/// session directory is.
pub(crate) fn read_manifest(dir: &Path) -> std::io::Result<Manifest> {
    let mut s = String::new();
    File::open(dir.join("manifest.json"))?.read_to_string(&mut s)?;
    serde_json::from_str(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Replace a session's manifest atomically: write a sibling, `fsync` it, then
/// `rename` it into place, so a crash can never leave a half-written manifest
/// — the one file that says what everything else in the directory means.
pub(crate) fn write_manifest_at(dir: &Path, manifest: &Manifest) -> std::io::Result<()> {
    let tmp = dir.join("manifest.json.tmp");
    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    {
        let mut f = create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, dir.join("manifest.json"))
}

/// Encode one leg.
///
/// Three shape problems get solved here, in an order that is not arbitrary:
///
/// * **Channels.** Opus tracks are mono (§9.5), so a multi-channel session is
///   averaged down first. Downmixing *before* the resampler halves the work
///   the filter has to do, which is the same reasoning as
///   [`crate::resample`]'s.
/// * **Rate.** libopus accepts only 8/12/16/24/48 kHz. The pipeline's own rate
///   is 16 kHz so the common path resamples nothing; anything else is put
///   through [`Resampler16k`] rather than rejected, because a session
///   recovered from an older or stranger capture must still be archivable.
///   That path loses up to one 10 ms chunk at the very end — the residue the
///   resampler is still holding — which is below the resolution of anything
///   downstream.
/// * **Framing.** The encoder re-blocks into 20 ms frames itself, so the read
///   size here is chosen for I/O efficiency alone.
fn encode_track(
    dir: &Path,
    pcm_name: &str,
    opus_name: &str,
    manifest: &Manifest,
) -> Result<EncodedTrack, OpusError> {
    let pcm_path = dir.join(pcm_name);
    let opus_path = dir.join(opus_name);
    let pcm_bytes = std::fs::metadata(&pcm_path).map(|m| m.len()).unwrap_or(0);

    let channels = manifest.channels.max(1);
    let session_rate = manifest.sample_rate_hz.max(1);
    let (encode_rate, mut resampler) = if SUPPORTED_RATES.contains(&session_rate) {
        (session_rate, None)
    } else {
        let r = Resampler16k::new(session_rate, 1)
            .map_err(|e| OpusError::Container(format!("cannot resample {session_rate} Hz: {e}")))?;
        (TARGET_RATE, Some(r))
    };

    let mut writer = OpusOggWriter::create(&opus_path, encode_rate)?;

    if pcm_bytes > 0 {
        let mut rdr = BufReader::new(File::open(&pcm_path)?);
        let mut bytes = vec![0u8; TRANSCODE_CHUNK_SAMPLES * BYTES_PER_SAMPLE as usize];
        // Bytes that did not make up a whole frame, carried to the next read.
        // A hard kill leaves a torn trailing frame; it is dropped at the end,
        // exactly as `SessionState::read` drops it.
        let mut carry: Vec<u8> = Vec::new();
        let frame_bytes = BYTES_PER_SAMPLE as usize * channels as usize;
        let mut last_sync = Instant::now();

        loop {
            let n = rdr.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            carry.extend_from_slice(&bytes[..n]);
            let usable = carry.len() / frame_bytes * frame_bytes;
            if usable == 0 {
                continue;
            }

            let interleaved: Vec<f32> = carry[..usable]
                .chunks_exact(BYTES_PER_SAMPLE as usize)
                .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0)
                .collect();
            carry.drain(..usable);

            let mono = Downmixer::to_mono(&interleaved, channels);
            match resampler.as_mut() {
                Some(r) => {
                    let out = r
                        .process_all(&mono)
                        .map_err(|e| OpusError::Container(e.to_string()))?;
                    writer.push_f32(&out)?;
                }
                None => writer.push_f32(&mono)?,
            }
            // Pages already reach the kernel on every page boundary, which is
            // what defeats `kill -9`. This is the other half — fsync on the
            // WAL's own cadence, which defeats a power cut — kept on a clock
            // rather than a chunk count so a fast disk does not turn a
            // transcode into thousands of fsyncs.
            if last_sync.elapsed() >= SYNC_INTERVAL {
                writer.sync()?;
                last_sync = Instant::now();
            }
        }
    }

    let stats = writer.finish()?;
    let opus_bytes = std::fs::metadata(&opus_path).map(|m| m.len()).unwrap_or(0);

    Ok(EncodedTrack {
        file: opus_name.to_string(),
        pcm_bytes,
        opus_bytes,
        duration_ms: stats.duration_ms(),
        sample_rate_hz: encode_rate,
    })
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
