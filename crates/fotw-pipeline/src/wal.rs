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
use crate::resample::{ArchiveConverter, Downmixer, TARGET_RATE};

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

/// The manifest schema this build writes.
///
/// Also the first schema that records a format per leg, which is what makes
/// an absent `mic_format` readable: at 4 or above it means the mic tap never
/// ran, below it means the writer did not know per-track formats existed and
/// the mic's real shape has to be inferred (#80).
const SCHEMA: u32 = 4;

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

/// The shape of one PCM leg on disk.
///
/// Per leg, because the two legs are two devices: the macOS system tap is
/// 48 kHz stereo and a mic is whatever the user plugged in. One count for both
/// is #80 — a mono mic file read as stereo turns every pair of consecutive
/// samples into one frame, so the archived track is exactly half its real
/// length and plays at 2×.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackFormat {
    /// Sample rate of this leg's PCM.
    pub sample_rate_hz: u32,
    /// Interleaved channels in this leg's PCM.
    pub channels: u16,
}

impl TrackFormat {
    /// One leg's format.
    #[must_use]
    pub const fn new(sample_rate_hz: u32, channels: u16) -> Self {
        Self {
            sample_rate_hz,
            channels,
        }
    }

    /// The same format with the zeroes taken out.
    ///
    /// A zero in either field is a manifest no reader can honour — it divides
    /// by frame size — and refusing the session over it would lose the
    /// meeting, so the safe interpretation is substituted instead.
    fn sane(self) -> Self {
        Self {
            sample_rate_hz: self.sample_rate_hz.max(1),
            channels: self.channels.max(1),
        }
    }
}

/// How a session's two PCM legs are to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFormats {
    /// Format of `system.pcm`.
    pub system: TrackFormat,
    /// Format of `mic.pcm`.
    pub mic: TrackFormat,
    /// Whether the mic's format was worked out rather than read.
    ///
    /// True for a manifest written before schema 4, which recorded one
    /// session-wide count for both legs. Worth surfacing: a wrong guess here
    /// is silent, and silence is how #80 shipped.
    pub mic_inferred: bool,
}

/// Everything needed to interpret the PCM files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Session id, also the directory name.
    pub id: String,
    /// Sample rate of `system.pcm`.
    ///
    /// Retained alongside [`system_format`](Self::system_format) so a reader
    /// that predates schema 4 still finds what it expects.
    pub sample_rate_hz: u32,
    /// Channels in `system.pcm`. As above: kept for older readers.
    pub channels: u16,
    /// Wall clock at session start, for display.
    pub started_at_ms: u64,
    /// Monotonic host clock at session start, for alignment.
    pub host_epoch_ns: u64,
    /// Version that wrote this, so a future reader can adapt.
    pub app_version: String,
    /// Schema version of this manifest.
    pub schema: u32,
    /// Format of `system.pcm`, from schema 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_format: Option<TrackFormat>,
    /// Format of `mic.pcm`, from schema 4.
    ///
    /// Absent at schema 4 or above means no mic tap ran and the file is
    /// empty; absent below it means the writer recorded only the one
    /// session-wide count, and the mic's real shape has to be inferred. Both
    /// are resolved by [`Manifest::track_formats`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_format: Option<TrackFormat>,
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

impl Manifest {
    /// The formats this session's two PCM legs are to be read with.
    ///
    /// The directory is a parameter because a pre-schema-4 manifest does not
    /// contain the answer: it recorded one channel count for the session and
    /// it was the system tap's, so the mic's has to come from the files
    /// themselves — see [`infer_mic_channels`].
    #[must_use]
    pub fn track_formats(&self, dir: &Path) -> SessionFormats {
        let global = TrackFormat::new(self.sample_rate_hz, self.channels).sane();
        let system = self.system_format.map_or(global, TrackFormat::sane);

        if let Some(mic) = self.mic_format {
            return SessionFormats {
                system,
                mic: mic.sane(),
                mic_inferred: false,
            };
        }
        if self.schema >= SCHEMA {
            // A writer that knows about per-track formats and recorded none
            // for the mic is saying the mic tap never ran. The file is empty,
            // so any format reads it identically.
            return SessionFormats {
                system,
                mic: global,
                mic_inferred: false,
            };
        }

        let bytes = |name: &str| {
            match std::fs::metadata(dir.join(name)) {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Retired PCM still has its original byte counts in the
                    // completed encode. Preserve the same legacy inference on
                    // retries instead of treating both legs as empty files.
                    self.encoded.as_ref().map_or(0, |encoded| {
                        if name == LEGS[0].0 {
                            encoded.system.pcm_bytes
                        } else {
                            encoded.mic.pcm_bytes
                        }
                    })
                }
                Err(_) => 0,
            }
        };
        SessionFormats {
            system,
            mic: TrackFormat {
                channels: infer_mic_channels(global.channels, bytes(LEGS[0].0), bytes(LEGS[1].0)),
                ..global
            },
            mic_inferred: true,
        }
    }
}

/// Work out a pre-schema-4 mic leg's channel count from the two legs' sizes.
///
/// The legs cover the same wall clock, so their byte counts stand in the ratio
/// of their channel counts: `mic_bytes ≈ sys_bytes × mic_ch / sys_ch`. A mic
/// file at half a stereo system's bytes was mono, which is what every session
/// on disk from before #80 turns out to be.
///
/// Deliberately conservative. Only the one case the evidence actually
/// distinguishes — a mic leg within a tenth of the size a mono leg would be —
/// overrides the recorded count; anything else keeps it, because reading a
/// genuinely stereo leg as mono is the same bug pointed the other way and the
/// two legs' sizes are not guaranteed to match (the mic tap starts second, and
/// a stalled tap leaves its leg short).
///
/// # The two cases it cannot answer (#80, written down by #89)
///
/// * **No denominator.** An empty or missing `system.pcm` means there is no
///   ratio to take, so the guard below hands back the recorded count untouched.
///   Not hypothetical: one such directory exists in the first real library. It
///   is the right answer — a count with no evidence against it beats a coin
///   toss — but it means a pre-schema-4 session whose system leg died is
///   encoded on the manifest's word alone.
/// * **A stereo mic that captured half a meeting.** Byte count is the only
///   evidence on disk, and half a stereo leg is byte-for-byte a whole mono leg.
///   A mic that stalled badly enough to land in the band is therefore misread,
///   and nothing on disk would say otherwise. That is precisely why
///   [`encode_session`] logs the choice rather than making it quietly: the
///   PCM is unlinked afterwards, so the log line is the only surviving record
///   of which way the guess went.
fn infer_mic_channels(global_channels: u16, sys_bytes: u64, mic_bytes: u64) -> u16 {
    if global_channels < 2 || sys_bytes == 0 || mic_bytes == 0 {
        return global_channels.max(1);
    }
    let ratio = mic_bytes as f64 / sys_bytes as f64;
    let mono_ratio = 1.0 / f64::from(global_channels);
    if (ratio - mono_ratio).abs() <= mono_ratio * 0.1 {
        1
    } else {
        global_channels
    }
}

/// One transcoded leg of a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodedTrack {
    /// Relative path within the session directory: a legacy `system.opus` or
    /// the same source name inside an immutable `.archive-<identity>` directory.
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
    capture_owner: crate::capture_clock::CaptureOwner,
    capture_checkpoint: Option<crate::capture_clock::CaptureCheckpoint>,
}

impl SessionWal {
    /// Persist a raw prefix and its original live clock; Stop freezes the first end.
    pub fn checkpoint_capture(
        &mut self,
        elapsed_ms: u64,
        stop_requested: bool,
    ) -> std::io::Result<()> {
        use crate::capture_clock::CaptureCheckpoint;
        let previous = self.capture_checkpoint.as_ref();
        let frozen = previous.filter(|p| p.stop_requested);
        let elapsed_ms = frozen.map_or(elapsed_ms, |p| p.elapsed_ms);
        if self
            .manifest
            .started_at_ms
            .checked_add(elapsed_ms)
            .is_none()
            || (!stop_requested && previous.is_some_and(|p| elapsed_ms < p.elapsed_ms))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "capture clock overflow or regression",
            ));
        }
        // A Stop timestamp may be published after a newer live observation
        // when its caller was descheduled. The first Stop is authoritative;
        // only live observations must advance monotonically.
        let stop_requested = stop_requested || frozen.is_some();
        self.flush()?;
        let checkpoint = CaptureCheckpoint {
            schema: 1,
            session_id: self.manifest.id.clone(),
            started_at_ms: self.manifest.started_at_ms,
            elapsed_ms,
            stop_requested,
            system_bytes: self.system.get_ref().metadata()?.len(),
            mic_bytes: self.mic.get_ref().metadata()?.len(),
            system_format: self.manifest.system_format.unwrap(),
            mic_format: self.manifest.mic_format,
        };
        let aligned = |bytes: u64, format: Option<TrackFormat>| {
            format.map_or(bytes == 0, |f| {
                f.channels > 0
                    && f.sample_rate_hz > 0
                    && bytes.is_multiple_of(u64::from(f.channels) * 2)
            })
        };
        if !aligned(checkpoint.system_bytes, Some(checkpoint.system_format))
            || !aligned(checkpoint.mic_bytes, checkpoint.mic_format)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "capture checkpoint has invalid PCM format or alignment",
            ));
        }
        self.capture_owner.publish(&checkpoint)?;
        self.capture_checkpoint = Some(checkpoint);
        Ok(())
    }

    /// Create a new session directory under `root` for two legs of the same
    /// shape.
    ///
    /// A session whose taps disagree — the ordinary case, where the system tap
    /// is stereo and the mic is mono — must use
    /// [`create_with_formats`](Self::create_with_formats) instead. One count
    /// applied to both legs is #80.
    pub fn create(
        root: impl AsRef<Path>,
        sample_rate_hz: u32,
        channels: u16,
    ) -> std::io::Result<Self> {
        let format = TrackFormat::new(sample_rate_hz, channels);
        Self::create_with_formats(root, format, Some(format))
    }

    /// Create a new session directory under `root`, recording each leg's own
    /// format.
    ///
    /// `mic` is optional because a meeting with no mic tap is an ordinary
    /// meeting: the leg exists, stays empty, and has no format to record.
    pub fn create_with_formats(
        root: impl AsRef<Path>,
        system: TrackFormat,
        mic: Option<TrackFormat>,
    ) -> std::io::Result<Self> {
        let (id, dir) = reserve_session_dir(root.as_ref(), session_id)?;
        let capture_owner = crate::capture_clock::CaptureOwner::create(&dir)?;

        let manifest = Manifest {
            id,
            sample_rate_hz: system.sample_rate_hz,
            channels: system.channels,
            started_at_ms: now_ms(),
            host_epoch_ns: 0,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            // 2 adds `encoded`; 3 adds `claim` and `promoted`; 4 adds
            // `system_format` and `mic_format`. Every one of those fields is
            // `#[serde(default)]`, so an older manifest still reads; the bump
            // exists to make the absence of a field distinguishable from a
            // writer that did not know about it.
            schema: SCHEMA,
            system_format: Some(system),
            mic_format: mic,
            ended_at_ms: None,
            gaps: Vec::new(),
            encoded: None,
            claim: None,
            promoted: None,
        };

        let mut wal = Self {
            system: BufWriter::new(create(&dir.join("system.pcm"))?),
            mic: BufWriter::new(create(&dir.join("mic.pcm"))?),
            stt: BufWriter::new(create(&dir.join("stt.jsonl"))?),
            manifest,
            dir,
            last_sync: Instant::now(),
            capture_owner,
            capture_checkpoint: None,
        };
        // Write the manifest immediately: a session that crashes one second in
        // must still be identifiable as a session.
        wal.write_manifest()?;
        wal.checkpoint_capture(0, false)?;
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
    /// Adjacent gaps with the same reason extend the existing marker, including
    /// when a different source's marker was added between updates.
    /// Gaps are written through to the manifest immediately: the whole point
    /// of a gap marker is to survive the crash that may follow the event that
    /// caused it.
    pub fn mark_gap(
        &mut self,
        start_ms: u64,
        end_ms: u64,
        reason: impl Into<String>,
    ) -> std::io::Result<()> {
        let reason = reason.into();
        // Incremental missing-audio padding extends one continuous outage,
        // rather than growing the manifest once per pump poll.
        if let Some(previous) = self
            .manifest
            .gaps
            .iter_mut()
            .rev()
            .find(|gap| gap.end_ms == start_ms && gap.reason == reason)
        {
            previous.end_ms = end_ms;
        } else {
            self.manifest.gaps.push(Gap {
                start_ms,
                end_ms,
                reason,
            });
        }
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

        // A frame is a frame of *this* leg: the mic is usually mono where the
        // system tap is stereo, and counting one with the other's frame size
        // reports half a meeting (#80).
        let formats = manifest.track_formats(&dir);
        // Integer division deliberately discards a partial trailing frame: a
        // kill mid-write leaves one, and rejecting the file over it would
        // throw away the entire meeting.
        let frames = |name: &str, format: TrackFormat| -> u64 {
            let frame_bytes = BYTES_PER_SAMPLE * u64::from(format.channels);
            std::fs::metadata(dir.join(name))
                .map(|m| m.len() / frame_bytes)
                .unwrap_or(0)
        };

        Ok(Self {
            system_frames: frames("system.pcm", formats.system),
            mic_frames: frames("mic.pcm", formats.mic),
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
/// `.pcm` still holds everything, so the encode can simply be re-run. A retry
/// writes a separate generation; the manifest atomically selects both tracks.
pub fn encode_session(dir: impl AsRef<Path>) -> Result<EncodedSession, OpusError> {
    let dir = dir.as_ref();
    let _owner = archive_owner(dir)?;
    let mut manifest = read_manifest(dir)?;

    // Validate both inputs before opening either output. Retrying an already
    // archived session must not truncate its last complete audio when raw PCM
    // was retired or shortened. An absent unrecorded microphone is ordinary.
    for (index, (pcm, opus)) in LEGS.iter().enumerate() {
        match std::fs::symlink_metadata(dir.join(opus)) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "archive destination is not an ordinary file",
                )
                .into());
            }
        }
        let minimum = manifest.encoded.as_ref().map_or(0, |encoded| {
            if index == 0 {
                encoded.system.pcm_bytes
            } else {
                encoded.mic.pcm_bytes
            }
        });
        match std::fs::symlink_metadata(dir.join(pcm)) {
            Ok(metadata) if metadata.file_type().is_file() && metadata.len() >= minimum => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && index == 1
                    && minimum == 0
                    && manifest.mic_format.is_none()
                    && matches!(std::fs::symlink_metadata(dir.join(opus)), Err(e) if e.kind()==std::io::ErrorKind::NotFound) =>
                {}
            Err(error) => return Err(error.into()),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "raw archive source is unavailable or shorter than the retained recording",
                )
                .into());
            }
        }
    }

    let formats = manifest.track_formats(dir);
    if formats.mic_inferred {
        // Said out loud because it is a guess with a permanent consequence:
        // the PCM is unlinked after this, and a mic track encoded at the
        // wrong channel count is half its real length forever. #80 shipped
        // because the wrong count was applied in silence.
        eprintln!(
            "  ! session {}: manifest schema {} records no per-track format; \
             reading mic.pcm as {} channel(s) against system.pcm's {} (#80)",
            manifest.id, manifest.schema, formats.mic.channels, formats.system.channels
        );
    }

    // Never overwrite an existing playable artifact, even an interrupted
    // first encode whose manifest has not yet recorded completion. Retrying
    // uses immutable generations so publishing one manifest commits both legs.
    let replacement =
        manifest.encoded.is_some() || LEGS.iter().any(|(_, name)| dir.join(name).exists());
    let mut generation = if replacement {
        let (_, path) = reserve_session_dir(dir, || format!(".archive-{}", session_id()))?;
        Some(ArchiveGeneration { path, keep: false })
    } else {
        None
    };
    let output = generation.as_ref().map_or(dir, |g| g.path.as_path());
    let mut system = encode_track(dir, output, LEGS[0].0, LEGS[0].1, formats.system)?;
    let mut mic = encode_track(dir, output, LEGS[1].0, LEGS[1].1, formats.mic)?;
    if let Some(generation) = &generation {
        let prefix = generation.path.file_name().unwrap().to_string_lossy();
        system.file = format!("{prefix}/system.opus");
        mic.file = format!("{prefix}/mic.opus");
        #[cfg(unix)]
        File::open(&generation.path)?.sync_all()?;
    }
    let encoded = EncodedSession { system, mic };

    manifest.encoded = Some(encoded.clone());
    // After publication is attempted a directory-sync error may mean the
    // new manifest is visible already. Keep both generations in that case;
    // never leave a manifest referring to files removed by error cleanup.
    if let Some(generation) = &mut generation {
        generation.keep = true;
    }
    write_manifest_at(dir, &manifest)?;
    Ok(encoded)
}

struct ArchiveGeneration {
    path: PathBuf,
    keep: bool,
}
impl Drop for ArchiveGeneration {
    fn drop(&mut self) {
        if !self.keep {
            for (_, name) in LEGS {
                let _ = std::fs::remove_file(self.path.join(name));
            }
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

/// Resolve only a source's legacy filename or an app-reserved generation.
/// Reject links and traversal before following a manifest-supplied path.
pub(crate) fn archive_path(dir: &Path, source: &str, file: &str) -> std::io::Result<PathBuf> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid archive source path",
        )
    };
    if file == source {
        return Ok(dir.join(source));
    }
    let Some((generation, name)) = file.split_once('/') else {
        return Err(invalid());
    };
    let Some(identity) = generation.strip_prefix(".archive-") else {
        return Err(invalid());
    };
    if name != source
        || identity.is_empty()
        || !identity.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        || !std::fs::symlink_metadata(dir.join(generation))?
            .file_type()
            .is_dir()
    {
        return Err(invalid());
    }
    Ok(dir.join(generation).join(name))
}

/// Unlink a session's raw PCM, returning the bytes reclaimed.
///
/// Requires two verified complete replacement archives and exact retained raw
/// byte counts. Validates both legs before removing either; extra raw samples,
/// corrupt archives or a live modern capture/recovery owner prevent deletion.
pub fn discard_pcm(dir: impl AsRef<Path>) -> std::io::Result<u64> {
    let dir = dir.as_ref();
    let _owner = archive_owner(dir)?;
    let manifest = read_manifest(dir)?;
    let encoded = manifest.encoded.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to unlink PCM: the manifest records no Opus encode",
        )
    })?;
    let formats = manifest.track_formats(dir);
    // Check BOTH replacement artifacts and raw counts before unlinking either
    // source. A new tail must be encoded again, never discarded as old audio.
    for ((_, opus), track, format) in [
        (LEGS[0], &encoded.system, formats.system),
        (LEGS[1], &encoded.mic, formats.mic),
    ] {
        verify_encoded_track(dir, opus, track, Some(format))?;
    }
    verify_retained_pcm(dir, encoded)?;
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

/// Refuse retirement of any raw source not exactly covered by the encode.
pub(crate) fn verify_retained_pcm(dir: &Path, encoded: &EncodedSession) -> std::io::Result<()> {
    for ((pcm, _), track) in [(LEGS[0], &encoded.system), (LEGS[1], &encoded.mic)] {
        match std::fs::symlink_metadata(dir.join(pcm)) {
            Ok(metadata) if metadata.file_type().is_file() && metadata.len() == track.pcm_bytes => {
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "raw audio differs from the completed archive",
                ));
            }
        }
    }
    Ok(())
}

/// Retain modern capture ownership throughout archive mutation. Legacy captures
/// without this sidecar still require full artifact verification before deletion;
/// never create a new sidecar as evidence of original recorder ownership.
pub(crate) fn archive_owner(
    dir: &Path,
) -> std::io::Result<Option<crate::capture_clock::CaptureOwner>> {
    match std::fs::symlink_metadata(dir.join("capture-owner.lock")) {
        Ok(metadata) if metadata.file_type().is_file() => {
            crate::capture_clock::CaptureOwner::recover(dir).map(Some)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "archive capture ownership is not an ordinary file",
        )),
    }
}

/// Verify the complete artifact against its original input frame count. Callers
/// bind `track.pcm_bytes` to their checkpoint or retained raw source separately.
pub(crate) fn verify_encoded_track(
    dir: &Path,
    name: &str,
    track: &EncodedTrack,
    format: Option<TrackFormat>,
) -> std::io::Result<()> {
    let invalid = |message| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    if !SUPPORTED_RATES.contains(&track.sample_rate_hz) {
        return Err(invalid("archive does not match its declared source"));
    }
    let path = archive_path(dir, name, &track.file)?;
    let encoded_frames = if let Some(format) = format {
        if format.sample_rate_hz == 0
            || format.channels == 0
            || !track
                .pcm_bytes
                .is_multiple_of(u64::from(format.channels) * 2)
        {
            return Err(invalid(
                "archive source format or frame alignment is invalid",
            ));
        }
        let expected_rate = if SUPPORTED_RATES.contains(&format.sample_rate_hz) {
            format.sample_rate_hz
        } else {
            TARGET_RATE
        };
        if track.sample_rate_hz != expected_rate {
            return Err(invalid("archive encoder rate differs from original format"));
        }
        u128::from(track.pcm_bytes / 2 / u64::from(format.channels))
            * u128::from(track.sample_rate_hz)
            / u128::from(format.sample_rate_hz)
    } else if track.pcm_bytes == 0 {
        0
    } else {
        return Err(invalid("nonempty archive has no original source format"));
    };
    let expected_ticks = u64::try_from(encoded_frames * u128::from(48000 / track.sample_rate_hz))
        .map_err(|_| invalid("archive sample count overflow"))?;
    if !std::fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err(invalid("archive must be an ordinary file"));
    }
    let mut options = crate::opus::archive_open_options();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() != track.opus_bytes {
        return Err(invalid("archive size differs from completion metadata"));
    }
    let observed = crate::opus::inspect_complete_archive(
        (&file).take(
            track
                .opus_bytes
                .checked_add(1)
                .ok_or_else(|| invalid("archive size overflow"))?,
        ),
    )
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if observed.bytes != track.opus_bytes
        || observed.input_rate_hz != track.sample_rate_hz
        || observed.pcm_samples_48k != expected_ticks
        || observed.duration_ms() != track.duration_ms
    {
        return Err(invalid("archive timing differs from original audio"));
    }
    file.sync_all()
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
        match std::fs::symlink_metadata(&tmp) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "manifest staging must be an ordinary file",
                ));
            }
        }
        let mut f = crate::opus::archive_open_options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, dir.join("manifest.json"))?;
    // On Unix the synced file alone does not persist newly created archive
    // names or the manifest rename. Publish the directory before raw retirement.
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Encode one leg.
///
/// Three shape problems get solved here, in an order that is not arbitrary:
///
/// * **Channels.** Opus tracks are mono (§9.5), so a multi-channel leg is
///   averaged down first. Downmixing *before* the resampler halves the work
///   the filter has to do, which is the same reasoning as
///   [`crate::resample`]'s. The count is this *leg's* — the two taps are two
///   devices, and applying one leg's count to the other is #80.
/// * **Rate.** libopus accepts only 8/12/16/24/48 kHz. The pipeline's own rate
///   is 16 kHz so the common path resamples nothing; anything else is put
///   through [`ArchiveConverter`] rather than rejected, because a session
///   recovered from an older or stranger capture must still be archivable.
///   Its final partial block is flushed before closing the encoder, retaining
///   the exact complete-frame duration at the destination rate.
/// * **Framing.** The encoder re-blocks into 20 ms frames itself, so the read
///   size here is chosen for I/O efficiency alone.
fn encode_track(
    dir: &Path,
    output: &Path,
    pcm_name: &str,
    opus_name: &str,
    format: TrackFormat,
) -> Result<EncodedTrack, OpusError> {
    let pcm_path = dir.join(pcm_name);
    let opus_path = output.join(opus_name);
    let pcm_bytes = std::fs::metadata(&pcm_path).map(|m| m.len()).unwrap_or(0);

    let TrackFormat {
        sample_rate_hz: track_rate,
        channels,
    } = format.sane();
    let (encode_rate, mut resampler) = if SUPPORTED_RATES.contains(&track_rate) {
        (track_rate, None)
    } else {
        let r = ArchiveConverter::new(track_rate, 1, TARGET_RATE, 1)
            .map_err(|e| OpusError::Container(format!("cannot resample {track_rate} Hz: {e}")))?;
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
                        .process(&mono)
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

    if let Some(resampler) = resampler.as_mut() {
        let tail = resampler
            .finish()
            .map_err(|e| OpusError::Container(e.to_string()))?;
        writer.push_f32(&tail)?;
    }
    let stats = writer.finish()?;
    // The completed manifest may authorize raw PCM retirement. Persist the
    // archive itself before publishing that manifest or deleting its source.
    crate::opus::archive_open_options()
        .read(true)
        .write(true)
        .open(&opus_path)?
        .sync_all()?;
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
fn reserve_session_dir(
    root: &Path,
    mut next_id: impl FnMut() -> String,
) -> std::io::Result<(String, PathBuf)> {
    std::fs::create_dir_all(root)?;
    for _ in 0..128 {
        let id = next_id();
        let dir = root.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a unique capture session directory",
    ))
}

fn session_id() -> String {
    static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The counter prevents same-process/same-clock collisions. Atomic directory
    // reservation remains the authority across processes, PID reuse and clocks.
    format!("{:013}-{:08x}-{serial:016x}", now_ms(), std::process::id())
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn occupied_identity_is_never_reused_even_when_the_generator_collides() {
        let root = std::env::temp_dir().join(format!("fotw-id-collision-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("occupied")).unwrap();
        std::fs::write(root.join("occupied/system.pcm"), b"preserved audio").unwrap();
        std::fs::write(root.join("occupied-file"), b"preserved file").unwrap();
        let mut ids = ["occupied", "occupied-file", "fresh"].into_iter();
        let (id, dir) = reserve_session_dir(&root, || ids.next().unwrap().into()).unwrap();
        assert_eq!(id, "fresh");
        assert_eq!(dir, root.join("fresh"));
        assert_eq!(
            std::fs::read(root.join("occupied/system.pcm")).unwrap(),
            b"preserved audio"
        );
        assert_eq!(
            std::fs::read(root.join("occupied-file")).unwrap(),
            b"preserved file"
        );
        let exhausted = reserve_session_dir(&root, || "occupied".into()).unwrap_err();
        assert_eq!(exhausted.kind(), std::io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(root).unwrap();
    }
}
