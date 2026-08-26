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
//!
//! # Surviving a tap that dies mid-meeting (CAP-05, CAP-06)
//!
//! A [`CaptureSupervisor`] watches each leg and rebuilds it when it stalls or
//! when the hardware moves. Three things about the wiring are load-bearing:
//!
//! **The supervisor is polled from the pump thread, not from a thread of its
//! own.** A rebuild produces a gap, and the silence that fills that gap has to
//! land in the PCM file *between* the samples either side of it. Only the
//! thread that owns the WAL can guarantee that ordering; a rebuild driven from
//! anywhere else races the pump and lands the padding in the wrong place.
//! Blocking the pump for the ~300 ms of a rebuild costs nothing, because the
//! tap is stopped for that whole window and nothing is arriving to drain.
//!
//! **The ring producer is recycled, not recreated.** [`ProducerSlot`] parks it
//! when the old sink is dropped and hands it to the new one, so a rebuilt tap
//! writes into the same ring, feeding the same WAL, and everything already on
//! disk is untouched.
//!
//! **An unwritten gap is padded with real silence, in that leg's own shape.**
//! The manifest's gap list records that those samples are synthetic, but the
//! samples themselves have to exist, or the byte offset of everything after
//! the gap no longer equals its session time and every later timestamp is
//! wrong. A second of a mono mic is half as many samples as a second of the
//! stereo system tap, so the two legs are padded from their own formats and
//! never from one session-wide count (#82).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use fotw_audio::clock::{Clock, HostClock};
use fotw_audio::supervisor::{
    CaptureGap, CaptureSupervisor, GapKind, HealthEvent, SupervisorConfig,
};
use fotw_audio::watchdog::{ActivityCounters, OutputActivity, TapActivity};
use fotw_audio::{
    AudioPlatform, CaptureTimestamp, DeviceId, FormatRequest, FrameFlags, FrameSink, PlatformProbe,
    StreamFormat, SystemScope, TapError, TapId, platform,
};
use fotw_pipeline::ring::{AudioRing, RingConsumer, RingProducer};
use fotw_pipeline::wal::{SessionFormats, SessionWal, TrackFormat};

/// Ring capacity. Ten seconds is generous — the pump drains every 100 ms —
/// but the cost is 1.3 MB and the benefit is surviving a stalled disk.
const RING_SECONDS: usize = 10;

/// How often the supervisor is asked to look at the tap.
///
/// Fast enough that the debounce window and the gap boundaries are accurate to
/// a fraction of a second, slow enough that a healthy meeting spends no
/// measurable time on it.
const SUPERVISE_INTERVAL: Duration = Duration::from_millis(100);

/// Silence is written in blocks of this many samples so a long gap does not
/// need a single allocation proportional to its length.
const PAD_CHUNK_SAMPLES: usize = 48_000;

/// The ring producer, parked between taps.
///
/// A rebuilt tap has to write into the **same** ring as the tap it replaced —
/// that is what makes recovery invisible to the WAL — but `AudioTap::start`
/// takes ownership of a sink and `stop` drops it. So the sink hands the
/// producer back here on drop, and the next sink takes it out again. Both ends
/// happen on the control path; the audio thread holds the producer outright
/// and never touches this lock.
#[derive(Clone)]
struct ProducerSlot(Arc<Mutex<Option<RingProducer>>>);

impl ProducerSlot {
    fn holding(producer: RingProducer) -> Self {
        Self(Arc::new(Mutex::new(Some(producer))))
    }

    fn take(&self) -> Option<RingProducer> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn put(&self, producer: RingProducer) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(producer);
    }

    /// A sink drawing on this slot.
    fn sink(&self, counters: Arc<ActivityCounters>, channels: u16) -> Box<dyn FrameSink> {
        Box::new(RingSink {
            producer: self.take(),
            slot: self.clone(),
            counters,
            channels: channels.max(1),
        })
    }
}

/// A sink that copies into the ring and does nothing else.
struct RingSink {
    /// `None` only if the slot was empty, which would mean the previous sink
    /// was still alive when this one was built. The supervisor drops the old
    /// tap before opening the new one, so it cannot happen — and if it ever
    /// did, counting the buffers keeps the watchdog from mistaking it for a
    /// stall and rebuilding in a loop.
    producer: Option<RingProducer>,
    slot: ProducerSlot,
    counters: Arc<ActivityCounters>,
    channels: u16,
}

impl Drop for RingSink {
    fn drop(&mut self) {
        if let Some(producer) = self.producer.take() {
            self.slot.put(producer);
        }
    }
}

impl FrameSink for RingSink {
    fn on_frames(&mut self, pcm: &[f32], _ts: CaptureTimestamp, flags: FrameFlags) {
        // Three relaxed atomic adds. This is the whole of the watchdog's
        // real-time footprint.
        self.counters.record(
            pcm.len() as u64 / u64::from(self.channels),
            flags.contains(FrameFlags::SILENT),
        );
        // Deliberately ignoring the return value. A short write means the
        // pump is behind; retrying here would be blocking by another name,
        // and the shortfall is already counted for the pump to surface.
        if let Some(producer) = self.producer.as_mut() {
            let _ = producer.push_block(pcm);
        }
    }

    fn on_error(&mut self, _e: TapError) {}
}

/// One supervised capture leg and everything the pump needs to service it.
struct Leg {
    label: &'static str,
    supervisor: CaptureSupervisor,
    counters: Arc<ActivityCounters>,
    consumer: RingConsumer,
    /// What the tap is delivering right now. Re-read after every rebuild.
    format: StreamFormat,
    /// The shape **this leg's** PCM is being written in, which is fixed for
    /// the session and is not necessarily the other leg's — a mono mic
    /// alongside a stereo system tap is the ordinary case. Padding a gap from
    /// the wrong one fills it with the wrong amount of silence (#82).
    wal_format: TrackFormat,
}

/// Where a leg's samples go in the WAL.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    System,
    Mic,
}

impl Channel {
    fn write(self, wal: &mut SessionWal, pcm: &[f32]) -> std::io::Result<()> {
        match self {
            Self::System => wal.write_system(pcm),
            Self::Mic => wal.write_mic(pcm),
        }
    }
}

/// Record system audio for `seconds` into a session under `root`.
pub fn record(root: PathBuf, seconds: u64) -> Result<PathBuf, String> {
    let plat = Arc::new(platform::host());
    let clock: Arc<dyn Clock> = Arc::new(HostClock);
    let capacity = 48_000 * 2 * RING_SECONDS;

    // Every allocation happens here, on this thread, before anything
    // real-time is running.
    let (sys_producer, sys_consumer) = AudioRing::with_capacity_frames(capacity);
    let sys_slot = ProducerSlot::holding(sys_producer);
    let sys_counters = Arc::new(ActivityCounters::new());

    let mut system = {
        let plat = Arc::clone(&plat);
        let slot = sys_slot.clone();
        let counters = Arc::clone(&sys_counters);
        // Channels are not known until start() reports the authoritative
        // format, and the sink needs them to count frames. Stereo is the
        // documented macOS tap shape and is only ever used for the counter's
        // frame total, which nothing branches on.
        CaptureSupervisor::new(
            SupervisorConfig {
                id: TapId::system_default(),
                ..SupervisorConfig::default()
            },
            Arc::clone(&clock),
            move || plat.open_system(SystemScope::DefaultOutputMix, FormatRequest::any()),
            move || slot.sink(Arc::clone(&counters), 2),
        )
    };
    let sys_format = system
        .start()
        .map_err(|e| format!("could not start the system tap: {e}"))?;

    // The listeners for CAP-06. The guard must outlive the recording: dropping
    // it unregisters them with no diagnostic anywhere.
    let _system_watch = match plat.watch_devices(system.signal()) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("  ! device-change notifications unavailable: {e}");
            eprintln!("    (the stall watchdog is still active)");
            None
        }
    };

    // The mic leg: a separate device, a separate IOProc and a separate ring.
    // Never fused into one aggregate — see fotw_audio::platform::macos::mic.
    let (mic_producer, mic_consumer) = AudioRing::with_capacity_frames(capacity);
    let mic_slot = ProducerSlot::holding(mic_producer);
    let mic_counters = Arc::new(ActivityCounters::new());
    let mut mic = {
        let plat = Arc::clone(&plat);
        let slot = mic_slot.clone();
        let counters = Arc::clone(&mic_counters);
        CaptureSupervisor::new(
            SupervisorConfig {
                id: TapId::mic("default"),
                ..SupervisorConfig::default()
            },
            Arc::clone(&clock),
            move || plat.open_mic(&DeviceId::new("default"), FormatRequest::any()),
            move || slot.sink(Arc::clone(&counters), 1),
        )
    };
    let mic_format = mic
        .start()
        .map_err(|e| eprintln!("  ! mic unavailable: {e}"))
        .ok();
    let _mic_watch = mic_format.and_then(|_| plat.watch_devices(mic.signal()).ok());

    println!("  system   : {sys_format}");
    match mic_format {
        Some(f) => println!("  mic      : {f}"),
        None => println!("  mic      : (not capturing)"),
    }
    println!("  session  : {}", root.display());

    // The session epoch on the same clock every tap stamps from, so a gap's
    // host-clock bounds convert to session-relative milliseconds by
    // subtraction. `SessionWal` does not record one itself.
    let session_epoch_ns = clock.now_ns();
    // A format per leg: the mic is its own device and usually mono where the
    // system tap is stereo, and one count applied to both is #80 — the
    // encoder reads the mono mic WAL as stereo and archives it at half its
    // real length.
    let mut wal = SessionWal::create_with_formats(
        &root,
        TrackFormat::new(sys_format.sample_rate_hz, sys_format.channels),
        mic_format.map(|f| TrackFormat::new(f.sample_rate_hz, f.channels)),
    )
    .map_err(|e| format!("could not create the session: {e}"))?;
    let dir = wal.dir().to_path_buf();
    let formats = wal_formats(&wal);

    let stop = Arc::new(AtomicBool::new(false));
    let pump_stop = Arc::clone(&stop);
    let pump_plat = Arc::clone(&plat);

    let mut legs = vec![(
        Channel::System,
        Leg {
            label: "system",
            supervisor: system,
            counters: Arc::clone(&sys_counters),
            consumer: sys_consumer,
            format: sys_format,
            wal_format: formats.system,
        },
    )];
    if let Some(format) = mic_format {
        legs.push((
            Channel::Mic,
            Leg {
                label: "mic",
                supervisor: mic,
                counters: Arc::clone(&mic_counters),
                consumer: mic_consumer,
                format,
                wal_format: formats.mic,
            },
        ));
    }

    // The pump: normal priority, does the I/O, drains on a 100 ms cadence, and
    // owns the supervisors so a rebuild's padding lands in the right place in
    // the stream. One pump drains BOTH rings; a thread per leg would double
    // the wakeups and buy nothing, since the writes go to one session anyway.
    let pump = std::thread::spawn(move || -> Result<Vec<(Channel, Leg, u64)>, String> {
        let mut scratch = vec![0.0f32; 48_000];
        let mut written = vec![0u64; legs.len()];
        let mut last_supervise = Instant::now();
        let mut stopping = false;

        loop {
            // Stop capture the moment the caller asks, then keep draining what
            // is already in the rings. Supervising a tap the user deliberately
            // ended would read its silence as a fault and rebuild it.
            if !stopping && pump_stop.load(Ordering::Acquire) {
                stopping = true;
                for (_, leg) in &mut legs {
                    let _ = leg.supervisor.stop();
                }
            }

            let mut moved = false;
            for (i, (channel, leg)) in legs.iter_mut().enumerate() {
                let n = leg.consumer.pop_into(&mut scratch);
                if n > 0 {
                    channel
                        .write(&mut wal, &scratch[..n])
                        .map_err(|e| format!("{} write failed: {e}", leg.label))?;
                    written[i] += n as u64;
                    moved = true;
                }
            }

            if !stopping && last_supervise.elapsed() >= SUPERVISE_INTERVAL {
                last_supervise = Instant::now();
                for (channel, leg) in &mut legs {
                    supervise(
                        &mut wal,
                        *channel,
                        leg,
                        &*pump_plat,
                        session_epoch_ns,
                        &mut scratch,
                    )?;
                }
            }

            if !moved {
                if stopping {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        wal.flush().map_err(|e| format!("flush failed: {e}"))?;
        let gaps = wal.manifest().gaps.len();
        wal.finalize()
            .map_err(|e| format!("finalize failed: {e}"))?;
        if gaps > 0 {
            println!("\n  ! {gaps} gap(s) recorded in the manifest");
        }
        Ok(legs
            .into_iter()
            .zip(written)
            .map(|((c, l), w)| (c, l, w))
            .collect())
    });

    let began = Instant::now();
    let target = Duration::from_secs(seconds);
    while began.elapsed() < target {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Ask the pump to stop, then let it drain what is still buffered. The
    // supervisors live on that thread now, so it stops the taps itself.
    stop.store(true, Ordering::Release);
    let legs = pump
        .join()
        .map_err(|_| "pump thread panicked".to_string())??;

    let mut system_total = 0u64;
    let mut system_silent = 0u64;
    for (channel, mut leg, written) in legs {
        let _ = leg.supervisor.stop();
        let activity = leg.counters.snapshot();
        let secs = written as f64
            / f64::from(leg.format.sample_rate_hz)
            / f64::from(leg.format.channels.max(1));
        println!(
            "\n  {:<9}: {written} samples ({secs:.2}s), {} buffers ({} silent), {} rebuild(s)",
            leg.label,
            activity.buffers,
            activity.silent_buffers,
            leg.supervisor.rebuilds()
        );
        if channel == Channel::System {
            system_total = activity.buffers;
            system_silent = activity.silent_buffers;
        }
    }

    if system_total == 0 {
        return Err("the IOProc never fired — the tap was not registered".into());
    }
    if system_silent == system_total {
        return Err(format!(
            "every one of {system_total} buffers was digitally silent.\n  \
             Either nothing was playing, or the system-audio permission was \
             denied — a denial delivers silence, not an error.\n  \
             Grant it under System Settings > Privacy & Security > Screen & \
             System Audio Recording, then:\n    \
             tccutil reset AudioCapture com.flyonthewall.fotw"
        ));
    }
    Ok(dir)
}

/// The shape each of the session's two PCM legs is written in.
///
/// Resolved through [`fotw_pipeline::wal::Manifest::track_formats`] rather
/// than read off the manifest's top-level `sample_rate_hz`/`channels`, which
/// are the *system* tap's (#80). There has to be exactly one answer to "what
/// shape is this leg", and this is the one the encoder, `SessionState` and
/// every other reader already use. A session being recorded is one this
/// process just wrote, so the resolver's pre-schema-4 inference never fires
/// here — the point is that the padder and the encoder cannot disagree, not
/// that this call site needs the legacy path.
fn wal_formats(wal: &SessionWal) -> SessionFormats {
    wal.manifest().track_formats(wal.dir())
}

/// Poll one leg's supervisor and apply whatever it decided to the WAL.
///
/// Runs on the pump thread, between drains, which is the only place a gap's
/// padding can be inserted in the right position in the stream.
fn supervise(
    wal: &mut SessionWal,
    channel: Channel,
    leg: &mut Leg,
    plat: &(impl AudioPlatform + ?Sized),
    epoch_ns: u64,
    scratch: &mut [f32],
) -> Result<(), String> {
    let activity: TapActivity = leg.counters.snapshot();
    // The mic leg gets no corroboration at all. "Is anything rendering
    // output?" is evidence about the *system* mixdown and says nothing about
    // an input device, so feeding it here would corroborate the mic's silence
    // with a fact about the speakers. `Unknown` disables the silence rule for
    // this leg and leaves starvation — which is the real mic failure — armed.
    let _ = match channel {
        Channel::System => leg.supervisor.poll(activity, &PlatformProbe(plat)),
        Channel::Mic => leg.supervisor.poll(activity, &OutputActivity::Unknown),
    };

    for event in leg.supervisor.drain_events() {
        if event.is_user_visible() {
            println!("  ! [{}] {}", leg.label, event.user_message());
        }
        let HealthEvent::Recovered { gaps, format, .. } = event else {
            continue;
        };

        if format != leg.format {
            // Re-reading the format after every rebuild is seam rule 1, and a
            // Bluetooth headset engaging HFP changes it mid-meeting. The WAL's
            // manifest declares one rate for the whole session, so this needs
            // a resampler on the pump before it is correct; saying so is
            // better than writing 16 kHz samples into a 48 kHz file quietly.
            println!(
                "  ! [{}] format changed across the rebuild: {} -> {format}. \
                 The manifest still records this leg as {} Hz / {} ch; its \
                 timing will be wrong until resampling is wired in.",
                leg.label, leg.format, leg.wal_format.sample_rate_hz, leg.wal_format.channels
            );
            leg.format = format;
        }

        // Everything still in the ring predates the outage: the tap was
        // stopped before the rebuild began, so nothing newer can be in there
        // yet. Draining it first is what puts the padding after the last
        // pre-gap sample instead of in the middle of it.
        loop {
            let n = leg.consumer.pop_into(scratch);
            if n == 0 {
                break;
            }
            channel
                .write(wal, &scratch[..n])
                .map_err(|e| format!("{} write failed: {e}", leg.label))?;
        }

        for gap in gaps {
            apply_gap(
                wal,
                channel,
                leg.label,
                &gap,
                leg.wal_format,
                epoch_ns,
                scratch,
            )?;
        }
    }
    Ok(())
}

/// Record a gap in the manifest and, if nothing was captured for it, put the
/// missing time back into the stream as silence.
fn apply_gap(
    wal: &mut SessionWal,
    channel: Channel,
    label: &str,
    gap: &CaptureGap,
    format: TrackFormat,
    epoch_ns: u64,
    scratch: &mut [f32],
) -> Result<(), String> {
    let start_ms = gap.start_ns.saturating_sub(epoch_ns) / 1_000_000;
    let end_ms = gap.end_ns.saturating_sub(epoch_ns) / 1_000_000;
    wal.mark_gap(start_ms, end_ms, format!("{label}: {}", gap.reason))
        .map_err(|e| format!("could not record the gap: {e}"))?;

    // A Silent gap's samples are already in the file. Padding it too would
    // push everything after the stall later by the length of the stall — the
    // same corruption as not padding an Unwritten one, in the other direction.
    if gap.kind == GapKind::Silent {
        return Ok(());
    }

    // Counted in *this* leg's interleaved samples. The gap's duration is the
    // same on both legs, but the samples that duration is worth are not: a
    // mono mic needs half what the stereo system tap does, and padding it with
    // the system tap's count doubles the silence and shifts everything after
    // it in that leg (#82).
    let mut remaining = gap
        .frames_to_pad(format.sample_rate_hz)
        .saturating_mul(u64::from(format.channels)) as usize;
    let chunk = PAD_CHUNK_SAMPLES.min(scratch.len());
    scratch[..chunk].fill(0.0);
    while remaining > 0 {
        let n = remaining.min(chunk);
        channel
            .write(wal, &scratch[..n])
            .map_err(|e| format!("could not pad the gap: {e}"))?;
        remaining -= n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_audio::AudioTap;
    use fotw_audio::supervisor::GapReason;
    use fotw_audio::testing::{FakeTap, ManualClock, MockPlatform};
    use fotw_pipeline::wal::SessionState;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fotw-record-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The shape a stereo system leg's PCM is written in.
    fn stereo48() -> TrackFormat {
        TrackFormat::new(48_000, 2)
    }

    /// One supervised leg over a scriptable tap, wired the way [`record`]
    /// wires a real one: its own ring, its own counters, its own supervisor —
    /// on a clock the test moves by hand, so a five-second stall costs no
    /// wall-clock time.
    fn fake_leg(
        label: &'static str,
        id: TapId,
        wal_format: TrackFormat,
        clock: &Arc<ManualClock>,
    ) -> Leg {
        let channels = wal_format.channels;
        let format = StreamFormat::new(
            wal_format.sample_rate_hz,
            channels,
            fotw_audio::SampleFormat::F32,
        );
        let (producer, consumer) = AudioRing::with_capacity_frames(48_000);
        let slot = ProducerSlot::holding(producer);
        let counters = Arc::new(ActivityCounters::new());
        let open_id = id.clone();
        let sink_slot = slot.clone();
        let sink_counters = Arc::clone(&counters);
        let mut supervisor = CaptureSupervisor::new(
            SupervisorConfig {
                id,
                ..SupervisorConfig::default()
            },
            Arc::clone(clock) as Arc<dyn Clock>,
            move || Ok(Box::new(FakeTap::new(open_id.clone(), format)) as Box<dyn AudioTap>),
            move || sink_slot.sink(Arc::clone(&sink_counters), channels),
        );
        supervisor.start().unwrap();
        Leg {
            label,
            supervisor,
            counters,
            consumer,
            format,
            wal_format,
        }
    }

    /// Issue #82: the gap padder ran on **both** legs with the system tap's
    /// format, so a mono mic's hole was filled with twice the silence it
    /// needed and everything after it in that leg sat at double its real
    /// offset.
    ///
    /// Asserted per leg, deliberately, and never on the total: a 2× overshoot
    /// on one leg is invisible in a sum across the two, which is exactly how
    /// this shipped.
    #[test]
    fn each_leg_pads_its_gap_with_its_own_channel_count() {
        let root = scratch_dir("per-leg-gap");
        let clock = ManualClock::new();
        let plat = MockPlatform::macos_taps();
        let mut scratch = vec![0.0f32; 48_000];

        // The ordinary macOS shape: a stereo system tap and a mono mic.
        let mut wal = SessionWal::create_with_formats(
            &root,
            TrackFormat::new(48_000, 2),
            Some(TrackFormat::new(48_000, 1)),
        )
        .unwrap();
        // Resolved the way `record` resolves them, and the way every reader
        // does: one answer per leg, not one answer for the session.
        let formats = wal_formats(&wal);
        assert_eq!(formats.system.channels, 2);
        assert_eq!(formats.mic.channels, 1, "the mic leg really is mono");

        let mut system = fake_leg("system", TapId::system_default(), formats.system, &clock);
        let mut mic = fake_leg("mic", TapId::mic("default"), formats.mic, &clock);

        // One second of audible audio down each leg, each at its own shape.
        Channel::System
            .write(&mut wal, &vec![0.5f32; 48_000 * 2])
            .unwrap();
        Channel::Mic.write(&mut wal, &vec![0.5f32; 48_000]).unwrap();

        // Five seconds in which neither tap delivered a buffer: both starve,
        // both rebuild, and both owe the stream five seconds of silence.
        clock.advance(Duration::from_secs(5));
        supervise(
            &mut wal,
            Channel::System,
            &mut system,
            &plat,
            0,
            &mut scratch,
        )
        .unwrap();
        supervise(&mut wal, Channel::Mic, &mut mic, &plat, 0, &mut scratch).unwrap();

        assert_eq!(system.supervisor.rebuilds(), 1, "the system tap rebuilt");
        assert_eq!(mic.supervisor.rebuilds(), 1, "and so did the mic");

        let dir = wal.finalize().unwrap();
        let state = SessionState::read(&dir).unwrap();

        // Bytes, per leg, because that is where the error lives: the mic's
        // padding is counted in samples and a mono second is half a stereo
        // one.
        let bytes = |name: &str| std::fs::metadata(dir.join(name)).unwrap().len();
        assert_eq!(
            bytes("system.pcm"),
            6 * 48_000 * 2 * 2,
            "1 s + 5 s of stereo silence"
        );
        assert_eq!(
            bytes("mic.pcm"),
            6 * 48_000 * 2,
            "1 s + 5 s of MONO silence, not the system tap's stereo"
        );

        // And therefore the acceptance criterion: the two legs cover the same
        // stretch of the meeting and end at the same moment.
        assert_eq!(state.system_frames, 6 * 48_000);
        assert_eq!(
            state.mic_frames, state.system_frames,
            "the legs must come out at equal duration"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Issue #25's acceptance criterion, at the only layer where it can be
    /// checked: after a recovery the recording is **one continuous artifact**
    /// whose byte offsets still line up with session time.
    ///
    /// The outage produced no samples at all. If the audio either side of it
    /// were simply concatenated, everything after the gap would sit five
    /// seconds early in the file, and nothing downstream — note anchors, STT
    /// offsets, the two-stream alignment — could detect it.
    #[test]
    fn an_unwritten_gap_is_padded_so_the_timeline_survives_it() {
        let root = scratch_dir("unwritten");
        let mut wal = SessionWal::create(&root, 48_000, 2).unwrap();
        let mut scratch = vec![0.0f32; 48_000];
        let epoch = 1_000_000_000u64;

        // One second of audible audio.
        let one_second = vec![0.5f32; 48_000 * 2];
        Channel::System.write(&mut wal, &one_second).unwrap();

        // Five seconds during which the tap was dead and nothing was written.
        let gap = CaptureGap {
            start_ns: epoch + 1_000_000_000,
            end_ns: epoch + 6_000_000_000,
            kind: GapKind::Unwritten,
            reason: GapReason::TapStalledNoBuffers,
        };
        apply_gap(
            &mut wal,
            Channel::System,
            "system",
            &gap,
            stereo48(),
            epoch,
            &mut scratch,
        )
        .unwrap();

        // One second more, from the rebuilt tap, into the same file.
        Channel::System.write(&mut wal, &one_second).unwrap();

        let manifest_gaps = wal.manifest().gaps.clone();
        let dir = wal.finalize().unwrap();

        let state = SessionState::read(&dir).unwrap();
        assert!(
            (state.system_seconds() - 7.0).abs() < 0.001,
            "expected 1 s + 5 s of padding + 1 s, got {} s",
            state.system_seconds()
        );

        // The gap is recorded as a gap, not merely filled in.
        assert_eq!(manifest_gaps.len(), 1);
        assert_eq!(manifest_gaps[0].start_ms, 1_000);
        assert_eq!(manifest_gaps[0].end_ms, 6_000);
        assert_eq!(manifest_gaps[0].duration_ms(), 5_000);
        assert!(manifest_gaps[0].reason.contains("tap-stall-no-buffers"));

        // And the padding really is silence sitting in the right place: the
        // audio either side of it is intact and was not rewritten.
        let pcm = std::fs::read(dir.join("system.pcm")).unwrap();
        let frame_bytes = 2 * 2; // i16, stereo
        let at = |sec: f64| -> i16 {
            let off = (sec * 48_000.0) as usize * frame_bytes;
            i16::from_le_bytes([pcm[off], pcm[off + 1]])
        };
        assert_ne!(at(0.5), 0, "the audio before the gap is still there");
        assert_eq!(at(1.5), 0, "the gap is silence");
        assert_eq!(at(5.5), 0, "all of it");
        assert_ne!(at(6.5), 0, "and the audio after it starts at 6 s, not 1 s");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other kind, and the corruption it would cause in the other
    /// direction. A silence stall's samples were captured — as zeros — so they
    /// are already in the file. Padding them again would push everything after
    /// the stall *later* by the length of the stall.
    #[test]
    fn a_silent_gap_is_recorded_but_never_padded() {
        let root = scratch_dir("silent");
        let mut wal = SessionWal::create(&root, 48_000, 2).unwrap();
        let mut scratch = vec![0.0f32; 48_000];

        // Eight seconds of digital silence that the tap really did deliver.
        let eight_seconds = vec![0.0f32; 48_000 * 2 * 8];
        Channel::System.write(&mut wal, &eight_seconds).unwrap();

        let gap = CaptureGap {
            start_ns: 0,
            end_ns: 8_000_000_000,
            kind: GapKind::Silent,
            reason: GapReason::TapStalledSilent,
        };
        apply_gap(
            &mut wal,
            Channel::System,
            "system",
            &gap,
            stereo48(),
            0,
            &mut scratch,
        )
        .unwrap();

        let gaps = wal.manifest().gaps.clone();
        let dir = wal.finalize().unwrap();
        let state = SessionState::read(&dir).unwrap();

        assert!(
            (state.system_seconds() - 8.0).abs() < 0.001,
            "the captured silence must not be duplicated: got {} s",
            state.system_seconds()
        );
        assert_eq!(gaps.len(), 1, "it is still recorded as lost content");
        assert!(gaps[0].reason.contains("tap-stall-silent"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
