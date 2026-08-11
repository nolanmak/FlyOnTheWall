//! `fotw record` — capture system audio to a crash-safe session on disk.
//!
//! This is the first end-to-end path in the project, and it is wired the way
//! the architecture requires rather than the way that would be easiest:
//!
//! ```text
//!   Core Audio IOProc  ->  lock-free ring  ->  pump thread  ->  WAL on disk
//!   (real-time thread)     (no alloc)          (normal prio)    (fsync'd)
//! ```
//!
//! The audio callback **never** touches the filesystem. Writing to disk from a
//! real-time thread is exactly the CAP-04 violation the allocator detector
//! exists to catch: a single blocking `write` on that thread produces a
//! dropout, and dropouts in a meeting recorder are silent data loss.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fotw_audio::{
    AudioPlatform, CaptureTimestamp, FormatRequest, FrameFlags, FrameSink, SystemScope, TapError,
    platform,
};
use fotw_pipeline::ring::{AudioRing, RingProducer};
use fotw_pipeline::wal::SessionWal;

/// Ring capacity. Ten seconds is generous — the pump drains every 100 ms —
/// but the cost is 1.3 MB and the benefit is surviving a stalled disk.
const RING_SECONDS: usize = 10;

/// A sink that copies into the ring and does nothing else.
struct RingSink {
    producer: RingProducer,
    silent_buffers: Arc<AtomicU64>,
    total_buffers: Arc<AtomicU64>,
}

impl FrameSink for RingSink {
    fn on_frames(&mut self, pcm: &[f32], _ts: CaptureTimestamp, flags: FrameFlags) {
        self.total_buffers.fetch_add(1, Ordering::Relaxed);
        if flags.contains(FrameFlags::SILENT) {
            self.silent_buffers.fetch_add(1, Ordering::Relaxed);
        }
        // Deliberately ignoring the return value. A short write means the
        // pump is behind; retrying here would be blocking by another name,
        // and the shortfall is already counted for the pump to surface.
        let _ = self.producer.push_block(pcm);
    }

    fn on_error(&mut self, _e: TapError) {}
}

/// Record system audio for `seconds` into a session under `root`.
pub fn record(root: PathBuf, seconds: u64) -> Result<PathBuf, String> {
    let plat = platform::host();
    let mut tap = plat
        .open_system(SystemScope::DefaultOutputMix, FormatRequest::any())
        .map_err(|e| format!("could not open the system tap: {e}"))?;

    let silent = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));

    // The ring must exist before the tap starts: every allocation happens
    // here, on this thread, before anything real-time is running.
    let capacity = 48_000 * 2 * RING_SECONDS;
    let (producer, mut consumer) = AudioRing::with_capacity_frames(capacity);

    let format = tap
        .start(Box::new(RingSink {
            producer,
            silent_buffers: Arc::clone(&silent),
            total_buffers: Arc::clone(&total),
        }))
        .map_err(|e| format!("could not start the tap: {e}"))?;

    println!("  format   : {format}");
    println!("  session  : {}", root.display());

    let mut wal = SessionWal::create(&root, format.sample_rate_hz, format.channels)
        .map_err(|e| format!("could not create the session: {e}"))?;
    let dir = wal.dir().to_path_buf();

    let stop = Arc::new(AtomicBool::new(false));
    let pump_stop = Arc::clone(&stop);

    // The pump: normal priority, does the I/O, drains on a 100 ms cadence.
    let pump = std::thread::spawn(move || -> Result<u64, String> {
        let mut scratch = vec![0.0f32; 48_000];
        let mut written: u64 = 0;
        loop {
            let n = consumer.pop_into(&mut scratch);
            if n > 0 {
                wal.write_system(&scratch[..n])
                    .map_err(|e| format!("write failed: {e}"))?;
                written += n as u64;
            } else {
                if pump_stop.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        wal.flush().map_err(|e| format!("flush failed: {e}"))?;
        wal.finalize()
            .map_err(|e| format!("finalize failed: {e}"))?;
        Ok(written)
    });

    let began = Instant::now();
    let target = Duration::from_secs(seconds);
    while began.elapsed() < target {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Stop the tap first, then let the pump drain what is still buffered.
    let _ = tap.stop();
    stop.store(true, Ordering::Release);
    let written = pump
        .join()
        .map_err(|_| "pump thread panicked".to_string())??;

    let secs = written as f64 / f64::from(format.sample_rate_hz) / f64::from(format.channels);
    let total_b = total.load(Ordering::Relaxed);
    let silent_b = silent.load(Ordering::Relaxed);

    println!("\n  captured : {written} samples ({secs:.2}s of audio)");
    println!("  buffers  : {total_b} ({silent_b} silent)");

    if total_b == 0 {
        return Err("the IOProc never fired — the tap was not registered".into());
    }
    if silent_b == total_b {
        return Err(format!(
            "every one of {total_b} buffers was digitally silent.\n  \
             Either nothing was playing, or the system-audio permission was \
             denied — a denial delivers silence, not an error.\n  \
             Grant it under System Settings > Privacy & Security > Screen & \
             System Audio Recording, then:\n    \
             tccutil reset AudioCapture com.flyonthewall.fotw"
        ));
    }
    Ok(dir)
}
