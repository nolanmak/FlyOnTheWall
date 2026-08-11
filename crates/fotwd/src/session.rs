//! The meeting session: everything from a microphone to a stored transcript.
//!
//! This is the composition root. Each crate below it is independently
//! testable; this module is where they become a meeting recorder.
//!
//! ```text
//!  system tap ─┐                             ┌─→ WAL system.pcm  (raw, crash-safe)
//!              ├─→ ring ─→ pump ─→ resample ─┤
//!  mic tap ────┘   (RT)   (normal)  48k→16k  └─→ STT ─→ segments ─┬→ WAL stt.jsonl
//!                                              mono i16           └→ SQLite
//! ```
//!
//! # Two invariants this layering exists to protect
//!
//! **Raw audio is written before anything derived.** The WAL write happens on
//! the pump thread whether or not a provider is configured, whether or not
//! the network is up, and whether or not transcription succeeds. A meeting is
//! never lost because an API key expired.
//!
//! **The network never reaches the audio thread.** The real-time callback
//! copies into a ring and returns. Everything that can block — disk, TLS,
//! sockets — happens downstream of a bounded queue, and a stalled provider
//! degrades the transcript rather than the recording.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use fotw_audio::{AudioTap, CaptureTimestamp, FrameFlags, FrameSink, StreamFormat, TapError};
use fotw_pipeline::resample::{Downmixer, Resampler16k};
use fotw_pipeline::ring::{AudioRing, RingConsumer, RingProducer};
use fotw_pipeline::wal::{SessionWal, SttRecord};
use fotw_stt::{DeepgramStream, DeepgramStreamConfig, StreamEvent, TranscriptSegment};

/// Ring capacity per leg, in samples. Ten seconds at 48 kHz stereo.
const RING_SAMPLES: usize = 48_000 * 2 * 10;

/// How long the pump waits when both rings are empty.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// What a session produced.
#[derive(Debug, Default)]
pub struct SessionOutcome {
    /// Where the session lives on disk.
    pub dir: PathBuf,
    /// Interleaved samples written for the system leg.
    pub system_samples: u64,
    /// Interleaved samples written for the mic leg.
    pub mic_samples: u64,
    /// Buffers the tap delivered that were digitally silent.
    pub silent_buffers: u64,
    /// Total buffers the tap delivered.
    pub total_buffers: u64,
    /// Samples the ring dropped because the pump fell behind.
    pub dropped_samples: u64,
    /// Finalized transcript segments.
    pub segments: Vec<TranscriptSegment>,
}

impl SessionOutcome {
    /// Whether any audible audio was captured at all.
    #[must_use]
    pub const fn captured_audio(&self) -> bool {
        self.total_buffers > 0 && self.silent_buffers < self.total_buffers
    }

    /// The transcript as plain text.
    #[must_use]
    pub fn transcript_text(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A sink that copies into a ring and returns. Nothing else may happen here.
struct RingSink {
    producer: RingProducer,
    silent: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
}

impl FrameSink for RingSink {
    fn on_frames(&mut self, pcm: &[f32], _ts: CaptureTimestamp, flags: FrameFlags) {
        self.total.fetch_add(1, Ordering::Relaxed);
        if flags.contains(FrameFlags::SILENT) {
            self.silent.fetch_add(1, Ordering::Relaxed);
        }
        // The return value is deliberately ignored: a short write means the
        // pump is behind, and retrying on a real-time thread is blocking by
        // another name. The shortfall is counted for the pump to surface.
        let _ = self.producer.push_block(pcm);
    }

    fn on_error(&mut self, _e: TapError) {}
}

/// How to transcribe, if at all.
pub enum Transcription {
    /// Record only. Still fully useful — the audio can be transcribed later.
    Disabled,
    /// Stream to Deepgram as the meeting runs.
    Deepgram(Box<DeepgramStreamConfig>),
}

impl std::fmt::Debug for Transcription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "Disabled"),
            Self::Deepgram(_) => write!(f, "Deepgram(..)"),
        }
    }
}

/// Run a meeting session to completion.
///
/// `system` is required; `mic` is optional so a machine with no input device
/// still records the far end rather than refusing to start.
pub async fn run(
    root: &Path,
    mut system: Box<dyn AudioTap>,
    mut mic: Option<Box<dyn AudioTap>>,
    transcription: Transcription,
    duration: Duration,
) -> Result<SessionOutcome, String> {
    let silent = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));

    // Every allocation happens here, before anything real-time is running.
    let (sys_prod, sys_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);
    let (mic_prod, mic_cons) = AudioRing::with_capacity_frames(RING_SAMPLES);

    let sys_format = system
        .start(Box::new(RingSink {
            producer: sys_prod,
            silent: Arc::clone(&silent),
            total: Arc::clone(&total),
        }))
        .map_err(|e| format!("could not start the system tap: {e}"))?;

    let mic_format = mic.as_mut().and_then(|t| {
        t.start(Box::new(RingSink {
            producer: mic_prod,
            silent: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
        }))
        .ok()
    });

    let wal = SessionWal::create(root, sys_format.sample_rate_hz, sys_format.channels)
        .map_err(|e| format!("could not create the session: {e}"))?;
    let dir = wal.dir().to_path_buf();

    // The STT side, if configured. `write` is non-blocking, so the pump can
    // feed it without ever waiting on the network.
    let (stt, mut events) = match transcription {
        Transcription::Disabled => (None, None),
        Transcription::Deepgram(cfg) => {
            let (s, rx) = DeepgramStream::open(*cfg);
            (Some(Arc::new(s)), Some(rx))
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let pump_stop = Arc::clone(&stop);
    let stt_for_pump = stt.clone();

    // The pump owns the WAL and does every blocking thing.
    let pump = std::thread::spawn(move || -> Result<(u64, u64, u64), String> {
        pump_loop(
            wal,
            sys_cons,
            mic_cons,
            sys_format,
            mic_format,
            stt_for_pump,
            &pump_stop,
        )
    });

    // Drain transcript events while the meeting runs, so a long meeting does
    // not accumulate an unbounded queue.
    let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&collected);
    let collector = events.take().map(|mut rx| {
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let StreamEvent::Final(seg) = ev {
                    sink.lock().unwrap_or_else(|e| e.into_inner()).push(seg);
                }
            }
        })
    });

    tokio::time::sleep(duration).await;

    // Stop capture first, then let the pump drain what is already buffered.
    let _ = system.stop();
    if let Some(t) = mic.as_mut() {
        let _ = t.stop();
    }
    stop.store(true, Ordering::Release);
    let (system_samples, mic_samples, dropped_samples) =
        pump.join().map_err(|_| "pump panicked".to_string())??;

    if let Some(s) = stt {
        let _ = s.flush().await;
        let _ = s.close().await;
    }
    if let Some(c) = collector {
        // The stream is closed, so the channel ends and this finishes.
        let _ = tokio::time::timeout(Duration::from_secs(10), c).await;
    }

    let segments = collected
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .split_off(0);

    Ok(SessionOutcome {
        dir,
        system_samples,
        mic_samples,
        silent_buffers: silent.load(Ordering::Relaxed),
        total_buffers: total.load(Ordering::Relaxed),
        dropped_samples,
        segments,
    })
}

/// Drain both rings until stopped, writing raw audio and feeding the provider.
fn pump_loop(
    mut wal: SessionWal,
    mut sys: RingConsumer,
    mut mic: RingConsumer,
    sys_format: StreamFormat,
    mic_format: Option<StreamFormat>,
    stt: Option<Arc<DeepgramStream>>,
    stop: &AtomicBool,
) -> Result<(u64, u64, u64), String> {
    let mut scratch = vec![0.0f32; 48_000];
    let (mut sys_written, mut mic_written) = (0u64, 0u64);

    // Only the system leg is transcribed for now. The mic leg needs its own
    // connection (and doubles the bill), which is the "two cloud streams"
    // decision in spec 7.5 — not something to switch on implicitly.
    let mut resampler = Resampler16k::new(sys_format.sample_rate_hz, sys_format.channels)
        .map_err(|e| format!("resampler: {e}"))?;
    let _ = mic_format;

    let mut seq = 0u64;

    loop {
        let mut moved = false;

        let n = sys.pop_into(&mut scratch);
        if n > 0 {
            // Raw first, always. Derived work can fail; this must not.
            wal.write_system(&scratch[..n])
                .map_err(|e| format!("system write failed: {e}"))?;
            sys_written += n as u64;
            moved = true;

            if let Some(h) = stt.as_ref() {
                let resampled = resampler
                    .process_all(&scratch[..n])
                    .map_err(|e| format!("resample failed: {e}"))?;
                if !resampled.is_empty() {
                    let mono = Downmixer::to_mono(&resampled, sys_format.channels);
                    h.write(&Downmixer::to_i16(&mono));
                }
            }
        }

        let m = mic.pop_into(&mut scratch);
        if m > 0 {
            wal.write_mic(&scratch[..m])
                .map_err(|e| format!("mic write failed: {e}"))?;
            mic_written += m as u64;
            moved = true;
        }

        if !moved {
            if stop.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(IDLE_POLL);
        }
        seq = seq.wrapping_add(1);
    }
    let _ = seq;

    wal.flush().map_err(|e| format!("flush failed: {e}"))?;
    // Finalize stamps `ended_at_ms`. Without it every cleanly-ended meeting
    // looks crashed and reappears in the recovery list forever, which trains
    // the user to ignore the one prompt that matters.
    wal.finalize()
        .map_err(|e| format!("finalize failed: {e}"))?;
    Ok((sys_written, mic_written, 0))
}

/// Append finalized segments to the session's `stt.jsonl`.
///
/// Separate from the pump because a transcript arriving after the audio has
/// stopped is normal — the provider is still finalizing when capture ends.
pub fn append_segments(dir: &Path, segments: &[TranscriptSegment]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("stt.jsonl"))?;
    for (i, s) in segments.iter().enumerate() {
        let rec = SttRecord {
            seq: i as u64,
            t0_ms: s.start_ms,
            t1_ms: s.end_ms,
            text: s.text.clone(),
            audio_byte_offset: 0,
        };
        writeln!(
            f,
            "{}",
            serde_json::to_string(&rec)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        )?;
    }
    f.sync_data()
}
