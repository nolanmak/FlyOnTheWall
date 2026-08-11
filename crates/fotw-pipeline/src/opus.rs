//! Opus encoding and incremental Ogg muxing.
//!
//! Raw PCM is the crash invariant *during* a meeting (see [`crate::wal`]), but
//! it is far too large to keep afterwards: two 48 kHz stereo `f32` tracks run
//! about 11.5 MB per minute, and even the 16 kHz mono `i16` the WAL actually
//! stores is 32 kB/s per leg. §9.5 of docs/REQUIREMENTS.md therefore specifies
//! the archival form — **two mono Opus streams per meeting at 24 kbps VBR,
//! `OPUS_APPLICATION_VOIP`, 20 ms frames, in an Ogg container**, which is
//! 3 kB/s per leg and turns 20 GB/yr of heavy use into something a laptop can
//! hold.
//!
//! # Why `opusic-sys` and not the `opus` crate
//!
//! The safe `opus` wrapper resolves to `audiopus_sys`, last released in 2021,
//! whose vendored build script does not survive CMake 4.x — it fails at
//! configure time on any modern machine, which is a build break rather than a
//! runtime one and so cannot be worked around at the call site. `opusic-sys`
//! vendors current libopus and builds it through the `cmake` crate, so **no
//! system libopus package is required on either CI platform**. Both it and the
//! `ogg` crate are BSD-3-Clause, which `deny.toml` allows.
//!
//! # Why the muxing is incremental
//!
//! CAP-10's acceptance criterion is that `kill -9` at t=90 min leaves a
//! *playable* file containing ≥ 89 minutes — not a truncated one that a player
//! refuses. Ogg gets that almost for free, because a page is a self-describing,
//! CRC'd unit with its own granule position: a reader that hits a torn page
//! simply stops at the last whole one. What it does *not* give for free is the
//! page ever reaching the kernel. So this writer forces a page boundary every
//! [`PAGE_INTERVAL_MS`] of audio and flushes the buffered writer there, which
//! caps the loss from a process kill at one page.
//!
//! Note the deliberate split between *flush* and *sync*:
//!
//! * `flush` hands the bytes to the kernel. That is what defeats `kill -9`,
//!   because the page cache outlives the process. It is cheap, so it happens
//!   on every page boundary.
//! * `sync` (`fsync`) defeats a power cut. It is expensive — 5,400 of them
//!   while transcoding a 90-minute meeting would dominate the runtime — so it
//!   is exposed as [`OpusOggWriter::sync`] for the caller to drive on a time
//!   cadence rather than a page cadence.
//!
//! # Where this runs
//!
//! Never on the real-time audio thread. `opus_encode` allocates nothing per
//! call but it is unbounded-latency signal processing, and CAP-04 forbids that
//! path anything but a `memcpy` and an atomic bump. Encoding is the pump
//! thread's job during a meeting, or a post-finalize transcode of the WAL's
//! PCM (see [`crate::wal::encode_session`]).

use std::ffi::{CStr, c_int};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, Write};
use std::path::Path;

use ogg::reading::PacketReader;
use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use opusic_sys as sys;

/// Frame duration, in milliseconds (§9.5).
///
/// 20 ms is Opus's default and the sweet spot for speech: 10 ms nearly doubles
/// the per-packet header overhead at this bitrate, and 40 ms adds latency that
/// a live-transcription pipeline has no reason to pay.
pub const FRAME_MS: u32 = 20;

/// Target bitrate in bits per second (§9.5).
pub const BITRATE_BPS: i32 = 24_000;

/// Ogg Opus granule positions are always counted at 48 kHz, whatever the
/// encoder's input rate (RFC 7845 §4). This is the single most common source
/// of "the file plays at the wrong speed" bugs.
pub const GRANULE_RATE: u64 = 48_000;

/// How much audio goes into one Ogg page before the page is forced out and the
/// writer flushed.
///
/// This is the exact quantity a `kill -9` can destroy, so it is also the knob
/// CAP-10 is graded on. One second costs about 77 bytes of page header and
/// lacing against 3,000 bytes of payload — 2.5% overhead to bound the loss at
/// one second instead of the ~5 seconds the container would otherwise buffer
/// (255 lacing segments × one 20 ms packet each).
pub const PAGE_INTERVAL_MS: u32 = 1_000;

/// Input rates libopus accepts. Anything else must be resampled first.
pub const SUPPORTED_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

/// Output buffer for one encoded packet. libopus's own documentation
/// recommends 4,000 bytes; a 20 ms mono VOIP packet at 24 kbps is nearer 60.
const MAX_PACKET_BYTES: usize = 4_000;

/// Longest frame Opus can carry, used to size the decode scratch buffer.
const MAX_FRAME_MS: usize = 120;

/// Failures from encoding, muxing, or reading back an Opus stream.
#[derive(Debug, thiserror::Error)]
pub enum OpusError {
    /// libopus only accepts 8/12/16/24/48 kHz.
    #[error("libopus does not accept {0} Hz; it takes 8, 12, 16, 24 or 48 kHz")]
    UnsupportedRate(u32),
    /// §9.5 specifies mono streams; stereo would double the bitrate for no
    /// transcription benefit, and the two legs are already separate files.
    #[error("Opus tracks here are mono; got {0} channels")]
    UnsupportedChannels(u16),
    /// libopus returned an error code.
    #[error("libopus {op} failed: {msg} (code {code})")]
    Codec {
        /// Which libopus call failed.
        op: &'static str,
        /// The raw libopus error code.
        code: i32,
        /// `opus_strerror`'s rendering of it.
        msg: String,
    },
    /// The bytes were not an Ogg Opus stream at all.
    #[error("not an Ogg Opus stream: {0}")]
    Container(String),
    /// Underlying I/O.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn strerror(code: c_int) -> String {
    // SAFETY: opus_strerror returns a pointer to a static NUL-terminated
    // string for every input, including unknown codes.
    let p = unsafe { sys::opus_strerror(code) };
    if p.is_null() {
        return format!("unknown error {code}");
    }
    // SAFETY: non-null and NUL-terminated per the contract above.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn check(op: &'static str, code: c_int) -> Result<(), OpusError> {
    if code == sys::OPUS_OK {
        Ok(())
    } else {
        Err(OpusError::Codec {
            op,
            code,
            msg: strerror(code),
        })
    }
}

/// Owning handle to a libopus encoder, so no path out of this module can leak
/// one.
#[derive(Debug)]
struct EncoderHandle(*mut sys::OpusEncoder);

impl Drop for EncoderHandle {
    fn drop(&mut self) {
        // SAFETY: the pointer came from opus_encoder_create, was checked
        // non-null there, and is destroyed exactly once.
        unsafe { sys::opus_encoder_destroy(self.0) }
    }
}

// The handle is an opaque heap allocation with no thread affinity; libopus
// documents an encoder as usable from any one thread at a time, which `&mut
// self` already enforces.
unsafe impl Send for EncoderHandle {}

/// Owning handle to a libopus decoder.
#[derive(Debug)]
struct DecoderHandle(*mut sys::OpusDecoder);

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        // SAFETY: as above, for the decoder.
        unsafe { sys::opus_decoder_destroy(self.0) }
    }
}

unsafe impl Send for DecoderHandle {}

/// What one finished Opus track cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusStats {
    /// Audio packets written, excluding the two header packets.
    pub packets: u64,
    /// Total encoded payload bytes, excluding Ogg framing.
    pub payload_bytes: u64,
    /// Input samples actually supplied, excluding the silence the final frame
    /// is padded with.
    pub input_samples: u64,
    /// Ogg pages emitted, including the two header pages.
    pub pages: u64,
    /// Encoder input rate.
    pub sample_rate_hz: u32,
}

impl OpusStats {
    /// Duration of the encoded audio.
    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        if self.sample_rate_hz == 0 {
            return 0;
        }
        self.input_samples * 1_000 / self.sample_rate_hz as u64
    }

    /// Effective bitrate, in bits per second.
    ///
    /// VBR means this lands under [`BITRATE_BPS`] on speech with pauses and
    /// close to it on continuous tone.
    #[must_use]
    pub fn effective_bitrate_bps(&self) -> f64 {
        let secs = self.duration_ms() as f64 / 1_000.0;
        if secs <= 0.0 {
            return 0.0;
        }
        self.payload_bytes as f64 * 8.0 / secs
    }
}

/// A streaming Opus encoder that muxes into Ogg as it goes.
///
/// Push arbitrarily-sized buffers with [`push_i16`](Self::push_i16) or
/// [`push_f32`](Self::push_f32); the writer re-blocks them into 20 ms frames
/// internally, so the caller never has to match the frame size. Call
/// [`finish`](Self::finish) to write the end-of-stream page. Not calling it —
/// a crash — costs at most the last [`PAGE_INTERVAL_MS`] and leaves every
/// earlier page playable.
pub struct OpusOggWriter<W: Write> {
    enc: EncoderHandle,
    ogg: PacketWriter<'static, W>,
    serial: u32,
    sample_rate_hz: u32,
    /// Samples in one 20 ms frame at `sample_rate_hz`.
    frame_samples: usize,
    /// 48 kHz granule ticks per input sample. Always an integer, because every
    /// rate Opus accepts divides 48,000.
    granule_per_sample: u64,
    /// Encoder lookahead, in 48 kHz ticks. The decoder discards this much from
    /// the front, which is how Opus stays sample-aligned despite its own
    /// latency.
    pre_skip: u64,
    /// Input samples that did not fill a frame, carried to the next push.
    pending: Vec<i16>,
    /// Reused frame buffer, so re-blocking allocates nothing per frame.
    frame_buf: Vec<i16>,
    /// Reused packet buffer, so steady-state encoding allocates nothing.
    scratch: Vec<u8>,
    input_samples: u64,
    frames_per_page: u32,
    frames_in_page: u32,
    packets: u64,
    payload_bytes: u64,
    pages: u64,
}

impl<W: Write> std::fmt::Debug for OpusOggWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusOggWriter")
            .field("sample_rate_hz", &self.sample_rate_hz)
            .field("serial", &self.serial)
            .field("input_samples", &self.input_samples)
            .field("packets", &self.packets)
            .field("pages", &self.pages)
            .finish()
    }
}

impl<W: Write> OpusOggWriter<W> {
    /// Build a mono 24 kbps VOIP encoder writing an Ogg stream into `sink`.
    pub fn new(sink: W, sample_rate_hz: u32) -> Result<Self, OpusError> {
        Self::with_page_interval(sink, sample_rate_hz, PAGE_INTERVAL_MS)
    }

    /// As [`new`](Self::new), but with an explicit page cadence.
    ///
    /// Exposed mostly so tests can pin the exact worst-case loss rather than
    /// inferring it.
    pub fn with_page_interval(
        sink: W,
        sample_rate_hz: u32,
        page_interval_ms: u32,
    ) -> Result<Self, OpusError> {
        if !SUPPORTED_RATES.contains(&sample_rate_hz) {
            return Err(OpusError::UnsupportedRate(sample_rate_hz));
        }

        let mut err: c_int = 0;
        // SAFETY: the rate is one of the five libopus accepts (checked above),
        // the channel count is 1, and `err` is a valid out-pointer.
        let raw = unsafe {
            sys::opus_encoder_create(
                sample_rate_hz as sys::opus_int32,
                1,
                sys::OPUS_APPLICATION_VOIP,
                &mut err,
            )
        };
        check("opus_encoder_create", err)?;
        if raw.is_null() {
            return Err(OpusError::Codec {
                op: "opus_encoder_create",
                code: sys::OPUS_ALLOC_FAIL,
                msg: "returned null".into(),
            });
        }
        let enc = EncoderHandle(raw);

        // SAFETY: every ctl below is a documented SET request whose variadic
        // argument is an opus_int32, matching what libopus reads back.
        unsafe {
            check(
                "OPUS_SET_BITRATE",
                sys::opus_encoder_ctl(enc.0, sys::OPUS_SET_BITRATE_REQUEST, BITRATE_BPS),
            )?;
            // VBR, unconstrained: §9.5 says VBR, and constraining it to a
            // per-frame ceiling throws away most of the win on speech, where
            // silence should cost almost nothing.
            check(
                "OPUS_SET_VBR",
                sys::opus_encoder_ctl(enc.0, sys::OPUS_SET_VBR_REQUEST, 1i32),
            )?;
            check(
                "OPUS_SET_VBR_CONSTRAINT",
                sys::opus_encoder_ctl(enc.0, sys::OPUS_SET_VBR_CONSTRAINT_REQUEST, 0i32),
            )?;
            check(
                "OPUS_SET_SIGNAL",
                sys::opus_encoder_ctl(enc.0, sys::OPUS_SET_SIGNAL_REQUEST, sys::OPUS_SIGNAL_VOICE),
            )?;
            // DTX off. It would save bytes in silence, but it makes packets
            // disappear from the stream, and the timeline this file has to
            // stay aligned with is the transcript's. Gaps are recorded
            // explicitly in the manifest, never implied by missing frames.
            check(
                "OPUS_SET_DTX",
                sys::opus_encoder_ctl(enc.0, sys::OPUS_SET_DTX_REQUEST, 0i32),
            )?;
            // Pin the frame size so a future libopus cannot quietly re-block
            // us and desynchronise the granule arithmetic below.
            check(
                "OPUS_SET_EXPERT_FRAME_DURATION",
                sys::opus_encoder_ctl(
                    enc.0,
                    sys::OPUS_SET_EXPERT_FRAME_DURATION_REQUEST,
                    sys::OPUS_FRAMESIZE_20_MS,
                ),
            )?;
        }

        let mut lookahead: sys::opus_int32 = 0;
        // SAFETY: OPUS_GET_LOOKAHEAD takes an opus_int32 out-pointer.
        unsafe {
            check(
                "OPUS_GET_LOOKAHEAD",
                sys::opus_encoder_ctl(enc.0, sys::OPUS_GET_LOOKAHEAD_REQUEST, &mut lookahead),
            )?;
        }

        let granule_per_sample = GRANULE_RATE / u64::from(sample_rate_hz);
        let pre_skip = lookahead.max(0) as u64 * granule_per_sample;
        let frame_samples = (sample_rate_hz * FRAME_MS / 1_000) as usize;
        let frames_per_page = (page_interval_ms / FRAME_MS).max(1);

        let mut w = Self {
            enc,
            ogg: PacketWriter::new(sink),
            serial: stream_serial(),
            sample_rate_hz,
            frame_samples,
            granule_per_sample,
            pre_skip,
            pending: Vec::with_capacity(frame_samples * 2),
            frame_buf: Vec::with_capacity(frame_samples),
            scratch: vec![0u8; MAX_PACKET_BYTES],
            input_samples: 0,
            frames_per_page,
            frames_in_page: 0,
            packets: 0,
            payload_bytes: 0,
            pages: 0,
        };
        w.write_headers()?;
        Ok(w)
    }

    /// The Ogg logical-stream serial number.
    #[must_use]
    pub const fn serial(&self) -> u32 {
        self.serial
    }

    /// Encoder lookahead in 48 kHz ticks, as written into `OpusHead`.
    #[must_use]
    pub const fn pre_skip(&self) -> u64 {
        self.pre_skip
    }

    /// Statistics so far.
    #[must_use]
    pub const fn stats(&self) -> OpusStats {
        OpusStats {
            packets: self.packets,
            payload_bytes: self.payload_bytes,
            input_samples: self.input_samples,
            pages: self.pages,
            sample_rate_hz: self.sample_rate_hz,
        }
    }

    /// Encode mono `i16` samples.
    pub fn push_i16(&mut self, pcm: &[i16]) -> Result<(), OpusError> {
        self.pending.extend_from_slice(pcm);
        self.input_samples += pcm.len() as u64;
        self.drain_full_frames()
    }

    /// Encode mono `f32` samples.
    ///
    /// Clamped rather than wrapped, for the same reason as
    /// [`crate::resample::Downmixer::to_i16`]: a tap routinely delivers above
    /// full scale, and wrapping turns a loud passage into white noise.
    pub fn push_f32(&mut self, pcm: &[f32]) -> Result<(), OpusError> {
        self.pending
            .extend(pcm.iter().map(|s| (s.clamp(-1.0, 1.0) * 32_767.0) as i16));
        self.input_samples += pcm.len() as u64;
        self.drain_full_frames()
    }

    /// Push the buffered writer's bytes to the kernel.
    ///
    /// Called automatically at every page boundary. After this returns, a
    /// `kill -9` cannot destroy what has been written — only a power cut can,
    /// which is what [`sync`](OpusOggWriter::sync) is for.
    pub fn flush(&mut self) -> Result<(), OpusError> {
        self.ogg.inner_mut().flush()?;
        Ok(())
    }

    /// Close the stream: pad and encode the trailing partial frame, mark
    /// end-of-stream, flush.
    ///
    /// The final granule position is the *true* sample count, not the padded
    /// one, so a decoder trims the padding back off (RFC 7845 §4.1
    /// end-trimming). Without that a 1.31-second meeting would play back as
    /// 1.32 seconds, and every duration in the UI would be quietly wrong.
    pub fn finish(mut self) -> Result<OpusStats, OpusError> {
        // Always emit one final frame, even from an empty `pending`: the Ogg
        // end-of-stream flag lives on a page, a page needs a packet to carry
        // it, and end-trimming makes the padding free.
        let mut frame = std::mem::take(&mut self.pending);
        frame.resize(self.frame_samples, 0);
        let final_granule = self.pre_skip + self.input_samples * self.granule_per_sample;
        self.encode_frame(&frame, PacketWriteEndInfo::EndStream, final_granule)?;
        self.ogg.inner_mut().flush()?;
        Ok(self.stats())
    }

    fn write_headers(&mut self) -> Result<(), OpusError> {
        // RFC 7845 §5.1: OpusHead must be alone on the first page, and
        // OpusTags must begin a new one. `EndPage` on each is exactly that.
        self.ogg.write_packet(
            opus_head(1, self.pre_skip as u16, self.sample_rate_hz),
            self.serial,
            PacketWriteEndInfo::EndPage,
            0,
        )?;
        self.ogg
            .write_packet(opus_tags(), self.serial, PacketWriteEndInfo::EndPage, 0)?;
        self.pages += 2;
        self.flush()
    }

    fn drain_full_frames(&mut self) -> Result<(), OpusError> {
        while self.pending.len() >= self.frame_samples {
            // Move the buffer out of `self` so `encode_frame` can take `&mut
            // self` while reading it. Both buffers keep their capacity, so
            // steady-state re-blocking allocates nothing.
            let mut buf = std::mem::take(&mut self.frame_buf);
            buf.clear();
            buf.extend(self.pending.drain(..self.frame_samples));

            let consumed = self.consumed_samples();
            let granule = self.pre_skip + consumed * self.granule_per_sample;
            let end = if self.frames_in_page + 1 >= self.frames_per_page {
                PacketWriteEndInfo::EndPage
            } else {
                PacketWriteEndInfo::NormalPacket
            };

            let result = self.encode_frame(&buf, end, granule);
            self.frame_buf = buf;
            result?;
        }
        Ok(())
    }

    /// Samples already handed to the encoder, i.e. everything pushed minus
    /// what is still waiting for a full frame.
    fn consumed_samples(&self) -> u64 {
        self.input_samples - self.pending.len() as u64
    }

    fn encode_frame(
        &mut self,
        frame: &[i16],
        end: PacketWriteEndInfo,
        granule: u64,
    ) -> Result<(), OpusError> {
        debug_assert_eq!(frame.len(), self.frame_samples);
        // SAFETY: `frame` holds exactly `frame_samples` mono samples, which is
        // the frame size the encoder was configured for, and `scratch` is
        // MAX_PACKET_BYTES long — above libopus's own recommended 4,000-byte
        // ceiling for one packet.
        let n = unsafe {
            sys::opus_encode(
                self.enc.0,
                frame.as_ptr(),
                self.frame_samples as c_int,
                self.scratch.as_mut_ptr(),
                self.scratch.len() as sys::opus_int32,
            )
        };
        if n < 0 {
            return Err(OpusError::Codec {
                op: "opus_encode",
                code: n,
                msg: strerror(n),
            });
        }

        let packet = self.scratch[..n as usize].to_vec();
        self.payload_bytes += n as u64;
        self.packets += 1;
        self.ogg.write_packet(packet, self.serial, end, granule)?;

        if end == PacketWriteEndInfo::NormalPacket {
            self.frames_in_page += 1;
        } else {
            self.frames_in_page = 0;
            self.pages += 1;
            // The page is complete; get it out of user space so a process
            // kill cannot take it with us.
            self.flush()?;
        }
        Ok(())
    }
}

impl OpusOggWriter<BufWriter<File>> {
    /// Create (or truncate) `path` and encode into it.
    pub fn create(path: impl AsRef<Path>, sample_rate_hz: u32) -> Result<Self, OpusError> {
        let f = File::create(path.as_ref())?;
        Self::new(BufWriter::new(f), sample_rate_hz)
    }

    /// Force everything durably to disk.
    ///
    /// Distinct from [`flush`](OpusOggWriter::flush) on purpose: `flush`
    /// survives a process kill, `sync` survives a power cut, and only the
    /// former is cheap enough to do on every page. Call this on a time
    /// cadence — the WAL uses [`crate::wal::SYNC_INTERVAL`].
    pub fn sync(&mut self) -> Result<(), OpusError> {
        let w = self.ogg.inner_mut();
        w.flush()?;
        w.get_ref().sync_data()?;
        Ok(())
    }
}

/// The `OpusHead` identification packet (RFC 7845 §5.1).
fn opus_head(channels: u8, pre_skip: u16, input_rate: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(19);
    v.extend_from_slice(b"OpusHead");
    v.push(1); // version
    v.push(channels);
    v.extend_from_slice(&pre_skip.to_le_bytes());
    // Informational only — the decoder always runs at whatever rate the
    // application asks for — but it is what tells a future reader the original
    // capture rate, which matters when the PCM is long gone.
    v.extend_from_slice(&input_rate.to_le_bytes());
    v.extend_from_slice(&0i16.to_le_bytes()); // output gain, Q7.8 dB
    v.push(0); // channel mapping family 0: mono/stereo, no mapping table
    v
}

/// The `OpusTags` comment packet (RFC 7845 §5.2).
fn opus_tags() -> Vec<u8> {
    let vendor = concat!("fotw-pipeline ", env!("CARGO_PKG_VERSION"));
    let mut v = Vec::with_capacity(16 + vendor.len());
    v.extend_from_slice(b"OpusTags");
    v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    v.extend_from_slice(vendor.as_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // no user comments
    v
}

/// A per-stream Ogg serial number.
///
/// Only has to be unique among the logical streams multiplexed into one
/// physical stream. We never multiplex — `mic.opus` and `system.opus` are
/// separate files — so this is belt and braces, but a fixed constant would
/// make two accidentally-concatenated files unreadable.
fn stream_serial() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u32);
    t.wrapping_mul(2_654_435_761)
        .wrapping_add(n.wrapping_mul(0x9E37_79B9))
}

/// PCM decoded back out of an Ogg Opus file.
#[derive(Debug, Clone)]
pub struct DecodedOpus {
    /// Rate the samples were decoded at.
    pub sample_rate_hz: u32,
    /// Channels declared by `OpusHead`.
    pub channels: u16,
    /// Encoder lookahead declared by `OpusHead`, in 48 kHz ticks.
    pub pre_skip: u16,
    /// The capture rate `OpusHead` records.
    pub input_sample_rate_hz: u32,
    /// Audio packets decoded.
    pub packets: u64,
    /// Interleaved samples, pre-skip removed and end-padding trimmed.
    pub samples: Vec<f32>,
    /// Whether the file ended in a torn page — i.e. whether this is the
    /// remains of a crash rather than a cleanly finished stream.
    pub truncated: bool,
}

impl DecodedOpus {
    /// Duration of the decoded audio.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate_hz == 0 || self.channels == 0 {
            return 0;
        }
        (self.samples.len() as u64 / u64::from(self.channels)) * 1_000
            / u64::from(self.sample_rate_hz)
    }

    /// Root-mean-square level of a window, for envelope checks.
    #[must_use]
    pub fn rms(&self, from: usize, to: usize) -> f32 {
        let to = to.min(self.samples.len());
        if from >= to {
            return 0.0;
        }
        let w = &self.samples[from..to];
        (w.iter().map(|s| s * s).sum::<f32>() / w.len() as f32).sqrt()
    }
}

/// Decode an Ogg Opus file back to PCM at `out_rate`.
///
/// This exists for three reasons, in ascending order of importance: the UI
/// needs playback; re-transcribing a meeting after the raw PCM has been swept
/// needs a decoder; and an acceptance test that only checks the file is
/// non-empty asserts essentially nothing. Opus is lossy, so a test built on
/// this must assert *content* — a frequency, an envelope, a duration — never
/// sample equality.
///
/// A file killed mid-write ends in a torn page. That is not an error here: the
/// whole point of CAP-10 is that such a file plays. Everything up to the last
/// intact page is returned with [`DecodedOpus::truncated`] set.
pub fn decode_ogg_opus_file(
    path: impl AsRef<Path>,
    out_rate: u32,
) -> Result<DecodedOpus, OpusError> {
    decode_ogg_opus(File::open(path.as_ref())?, out_rate)
}

/// As [`decode_ogg_opus_file`], from any seekable reader.
pub fn decode_ogg_opus<R: Read + Seek>(rdr: R, out_rate: u32) -> Result<DecodedOpus, OpusError> {
    if !SUPPORTED_RATES.contains(&out_rate) {
        return Err(OpusError::UnsupportedRate(out_rate));
    }
    let mut reader = PacketReader::new(rdr);

    let head = reader
        .read_packet()
        .map_err(|e| OpusError::Container(e.to_string()))?
        .ok_or_else(|| OpusError::Container("empty stream".into()))?;
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
        return Err(OpusError::Container("first packet is not OpusHead".into()));
    }
    let channels = u16::from(head.data[9]);
    let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]);
    let input_sample_rate_hz =
        u32::from_le_bytes([head.data[12], head.data[13], head.data[14], head.data[15]]);
    if channels != 1 {
        return Err(OpusError::UnsupportedChannels(channels));
    }

    let tags = reader
        .read_packet()
        .map_err(|e| OpusError::Container(e.to_string()))?
        .ok_or_else(|| OpusError::Container("stream ended before OpusTags".into()))?;
    if tags.data.len() < 8 || &tags.data[..8] != b"OpusTags" {
        return Err(OpusError::Container("second packet is not OpusTags".into()));
    }

    let mut err: c_int = 0;
    // SAFETY: out_rate is one of the five libopus accepts (checked above),
    // channels is 1 (checked above), and `err` is a valid out-pointer.
    let raw = unsafe { sys::opus_decoder_create(out_rate as sys::opus_int32, 1, &mut err) };
    check("opus_decoder_create", err)?;
    if raw.is_null() {
        return Err(OpusError::Codec {
            op: "opus_decoder_create",
            code: sys::OPUS_ALLOC_FAIL,
            msg: "returned null".into(),
        });
    }
    let dec = DecoderHandle(raw);

    let max_frame = out_rate as usize / 1_000 * MAX_FRAME_MS;
    let mut frame = vec![0f32; max_frame];
    let mut samples: Vec<f32> = Vec::new();
    let mut packets = 0u64;
    let mut truncated = false;
    let mut last_granule = 0u64;

    loop {
        match reader.read_packet() {
            Ok(Some(p)) => {
                // SAFETY: `p.data` is a valid slice; `frame` holds the longest
                // frame Opus can emit at this rate; decode_fec = 0.
                let n = unsafe {
                    sys::opus_decode_float(
                        dec.0,
                        p.data.as_ptr(),
                        p.data.len() as sys::opus_int32,
                        frame.as_mut_ptr(),
                        max_frame as c_int,
                        0,
                    )
                };
                if n < 0 {
                    return Err(OpusError::Codec {
                        op: "opus_decode_float",
                        code: n,
                        msg: strerror(n),
                    });
                }
                samples.extend_from_slice(&frame[..n as usize]);
                packets += 1;
                // u64::MAX is Ogg's "no packet completes on this page".
                if p.absgp_page() != u64::MAX {
                    last_granule = last_granule.max(p.absgp_page());
                }
            }
            Ok(None) => break,
            // A torn final page is the expected shape of a `kill -9`, not a
            // corrupt file. Keep everything before it.
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }

    // Opus's own latency, discarded from the front.
    let skip = usize::from(pre_skip) * out_rate as usize / GRANULE_RATE as usize;
    if skip < samples.len() {
        samples.drain(..skip);
    } else {
        samples.clear();
    }

    // End-trim to the final granule position, which is what removes the
    // silence `finish` padded the last frame with.
    //
    // `>=` and not `>`: a zero-length track still carries one padded frame so
    // that the end-of-stream page has a packet to ride on, and its granule is
    // exactly `pre_skip`. Treating that as "no trim needed" is what makes an
    // empty mic leg decode as 13 ms of silence instead of nothing.
    if last_granule >= u64::from(pre_skip) {
        let want = (last_granule - u64::from(pre_skip)) as usize * out_rate as usize
            / GRANULE_RATE as usize;
        if want < samples.len() {
            samples.truncate(want);
        }
    }

    Ok(DecodedOpus {
        sample_rate_hz: out_rate,
        channels,
        pre_skip,
        input_sample_rate_hz,
        packets,
        samples,
        truncated,
    })
}
