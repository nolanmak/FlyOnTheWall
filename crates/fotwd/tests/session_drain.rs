//! Where "finishing" ends (#85).
//!
//! #77 gave the dashboard an honest `Finishing` state. It did not give that
//! state an end: the tail of `session::run_with_control` is three blocking
//! calls on a tokio worker — `system.stop()`, `mic.stop()`, and the wait on
//! the pump — and none of them had a clock. A device that would not close, or
//! a pump that would not come back, therefore held the recorder's slot until
//! the daemon restarted, and every Start in between was refused with
//! `AlreadyRecording`.
//!
//! That is not a hypothetical device. `recording.rs` already carries a
//! user-facing error about a Core Audio HAL that "blocks rather than failing"
//! and tells the user to `sudo killall coreaudiod`; #77's `READY_DEADLINE`
//! exists because that HAL blocks in `start()`. It blocks in `stop()` for the
//! same reason, and it can also acknowledge a teardown it never performed.
//! The two fixtures below are those two devices.
//!
//! # Why nothing here waits on a tokio timer
//!
//! A blocking call on a runtime worker does not merely wedge its own task: a
//! worker parked inside one stops driving the runtime's time source, and every
//! `tokio::time` future in that runtime stops firing with it. A test that
//! bounded the bug with `tokio::time::timeout` would hang rather than fail —
//! it did, before these were rewritten. Every deadline below is a `std` one
//! taken on the `block_on` thread, which is the one clock the fault cannot
//! stop.
//!
//! # Why the deadlines are injected rather than waited out
//!
//! `FinishDeadlines::default()` is ten seconds each for closing the taps and
//! draining the rings, and thirty for the pump's answer — right for a laptop
//! and wrong for a suite. Each test overrides the one step it is about and
//! leaves the others at their defaults, so what is measured is the behaviour
//! at that deadline rather than the length of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fotw_audio::platform::file::{FileAudioSource, ReplaySpeed};
use fotw_audio::wav::WavData;
use fotw_audio::{
    AudioTap, CaptureTimestamp, FrameFlags, FrameSink, SampleFormat, StreamFormat, TapError, TapId,
};
use fotw_web::{RecorderControl, RecordingState};
use fotwd::recording::DaemonRecorder;
use fotwd::session::{self, FinishDeadlines, SessionControl, SessionOutcome, Transcription};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("fotwd-drain-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("sessions")).unwrap();
    base
}

fn capture_format() -> StreamFormat {
    StreamFormat::new(48_000, 2, SampleFormat::I16)
}

fn file_tap(seconds: f32) -> Box<dyn AudioTap> {
    let format = capture_format();
    let n = (48_000.0 * seconds) as usize;
    let mut samples = Vec::with_capacity(n * 2);
    for i in 0..n {
        let v = ((i as f32) / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        samples.push(v);
        samples.push(v);
    }
    Box::new(FileAudioSource::from_wav(
        TapId::system_default(),
        WavData { format, samples },
        ReplaySpeed::Realtime,
    ))
}

/// The session's outcome, over a channel a stalled time source cannot break.
type Finished = std::sync::mpsc::Receiver<Result<SessionOutcome, String>>;

/// Start a session on the runtime and hand back its two ends.
fn spawn_session(
    root: &std::path::Path,
    tap: Box<dyn AudioTap>,
    control: SessionControl,
) -> Finished {
    let (done, finished) = std::sync::mpsc::channel();
    let sessions = root.join("sessions");
    tokio::spawn(async move {
        let outcome = session::run_with_control(
            &sessions,
            tap,
            None,
            Transcription::Disabled,
            Duration::from_secs(3_600),
            control,
        )
        .await;
        let _ = done.send(outcome);
    });
    finished
}

// ------------------------------------------------------------------ fixtures

/// A tap that starts cleanly and then never returns from `stop()`.
///
/// The wedge #85 names: a HAL that still believes a dead client holds the
/// device blocks in `stop()` exactly as it blocks in `start()`, and nothing in
/// this process can cancel the syscall it is stuck in. Modelled on
/// `session_ready.rs`'s `WedgedTap`, pointed the other way.
///
/// The release flag exists only so the *test binary* can exit. A thread left
/// blocked forever would make the suite pass and then hang, which is a worse
/// failure than the bug.
struct NeverStoppingTap {
    id: TapId,
    released: Arc<AtomicBool>,
}

impl NeverStoppingTap {
    fn pair() -> (Box<dyn AudioTap>, Arc<AtomicBool>) {
        let released = Arc::new(AtomicBool::new(false));
        (
            Box::new(Self {
                id: TapId::system_default(),
                released: Arc::clone(&released),
            }),
            released,
        )
    }
}

impl AudioTap for NeverStoppingTap {
    fn id(&self) -> &TapId {
        &self.id
    }
    fn format(&self) -> StreamFormat {
        capture_format()
    }
    fn format_is_authoritative(&self) -> bool {
        true
    }
    fn start(&mut self, _sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        Ok(capture_format())
    }
    fn stop(&mut self) -> Result<(), TapError> {
        while !self.released.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

/// Ten milliseconds of 48 kHz stereo: what the fixture delivers while healthy.
const BLOCK_SAMPLES: usize = 960;

/// What the runaway delivers instead: a whole ring at a time.
///
/// `session.rs` sizes the ring at ten seconds of 48 kHz stereo. Pushing that
/// much per call is what makes the fixture *deterministically* faster than the
/// pump: a producer that merely kept pace would leave the ring empty between
/// passes, and an empty ring is the one case the pump always handled.
const RUNAWAY_SAMPLES: usize = 48_000 * 2 * 10;

/// How long the runaway pauses between deliveries.
const RUNAWAY_PACE: Duration = Duration::from_micros(200);

/// How many ring-fulls the runaway may deliver before it gives up.
///
/// The ceiling exists so that a *broken* pump cannot fill the disk: the WAL
/// can never take more than was pushed, so this caps the fixture at
/// `RUNAWAY_BLOCKS × RUNAWAY_SAMPLES × 2` bytes — tens of megabytes — however
/// long a regression would otherwise have kept draining. It is also four
/// milliseconds of runaway, which is several times the drain deadline any test
/// here uses; a run that exhausted it fails an assertion rather than hanging.
const RUNAWAY_BLOCKS: u64 = 20;

/// A tap whose `stop()` answers and whose delivery does not.
///
/// The other half of the same fault: the teardown is acknowledged and the
/// IOProc goes on firing, faster than the pump can write. It saturates the
/// ring on purpose — a runaway that merely kept pace would leave the ring
/// empty between passes, and an empty ring is the one case the pump always
/// handled.
struct RelentlessTap {
    id: TapId,
    stopped: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    /// Blocks delivered since `stop()`, so `stop()` can wait for the runaway
    /// to be genuinely under way before it answers.
    runaway: Arc<AtomicU64>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RelentlessTap {
    fn new() -> Self {
        Self {
            id: TapId::system_default(),
            stopped: Arc::new(AtomicBool::new(false)),
            released: Arc::new(AtomicBool::new(false)),
            runaway: Arc::new(AtomicU64::new(0)),
            worker: None,
        }
    }
}

impl AudioTap for RelentlessTap {
    fn id(&self) -> &TapId {
        &self.id
    }
    fn format(&self) -> StreamFormat {
        capture_format()
    }
    fn format_is_authoritative(&self) -> bool {
        true
    }
    fn start(&mut self, mut sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        let stopped = Arc::clone(&self.stopped);
        let released = Arc::clone(&self.released);
        let runaway = Arc::clone(&self.runaway);
        self.worker = Some(std::thread::spawn(move || {
            // Audible, so the leg does not additionally report itself silent
            // and muddy the assertion about the abandoned tail.
            let wave = |n: usize| -> Vec<f32> {
                (0..n).map(|i| ((i as f32) * 0.01).sin() * 0.5).collect()
            };
            let healthy = wave(BLOCK_SAMPLES);
            let runaway_block = wave(RUNAWAY_SAMPLES);
            let mut frames = 0u64;
            while !released.load(Ordering::Relaxed) {
                if stopped.load(Ordering::Acquire) {
                    if runaway.load(Ordering::Acquire) >= RUNAWAY_BLOCKS {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    frames += (runaway_block.len() / 2) as u64;
                    sink.on_frames(
                        &runaway_block,
                        CaptureTimestamp::new(frames, 0),
                        FrameFlags::empty(),
                    );
                    runaway.fetch_add(1, Ordering::Release);
                    std::thread::sleep(RUNAWAY_PACE);
                } else {
                    frames += (healthy.len() / 2) as u64;
                    sink.on_frames(
                        &healthy,
                        CaptureTimestamp::new(frames, 0),
                        FrameFlags::empty(),
                    );
                    // Ten times real time while the device is behaving, in
                    // slices short enough that `stop()` answers promptly.
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }));
        Ok(capture_format())
    }
    fn stop(&mut self) -> Result<(), TapError> {
        self.stopped.store(true, Ordering::Release);
        // Deterministic rather than hopeful: the session sets the pump's latch
        // the instant this returns, and a fixture still waking up would let
        // the pump find an empty ring and leave through the ordinary door,
        // which is not the case under test.
        while self.runaway.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        Ok(())
    }
}

impl Drop for RelentlessTap {
    fn drop(&mut self) {
        self.released.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

// ------------------------------------------------------------ a wedged close

/// The bug at the level the user meets it: a device that will not close must
/// not keep the session open forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tap_that_never_returns_from_stop_still_ends_the_session() {
    let root = tmpdir("wedged-stop");
    let (tap, released) = NeverStoppingTap::pair();
    let mut control = SessionControl::new();
    control.deadlines = FinishDeadlines {
        close: Duration::from_millis(200),
        ..FinishDeadlines::default()
    };
    let ready = control.ready.clone();
    let stop = control.stop.clone();

    let finished = spawn_session(&root, tap, control);
    assert!(
        ready.wait_timeout(Duration::from_secs(10)),
        "capture never went live, so there is nothing to stop"
    );
    stop.stop();

    let began = Instant::now();
    let ended = finished.recv_timeout(Duration::from_secs(20));
    let waited = began.elapsed();
    // Released before any assertion: a panic would drop the runtime, and a
    // runtime with a worker still inside `stop()` never finishes shutting
    // down. A failing test must fail, not hang.
    released.store(true, Ordering::Relaxed);

    let outcome = ended
        .unwrap_or_else(|_| {
            panic!(
                "a tap that never returns from stop() held the session open for \
                 {waited:?} — the recorder is dead until the daemon restarts (#85)"
            )
        })
        .expect("a device that will not close must not fail the meeting");

    // And it says so. A recorder that quietly frees itself is its own kind of
    // lie: the device is still open, and only the user can clear it.
    assert!(
        outcome
            .stt_errors
            .iter()
            .any(|e| e.contains("did not close") && e.contains("coreaudiod")),
        "the abandoned device was not reported: {:?}",
        outcome.stt_errors
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The acceptance criterion, at the recorder: the slot frees, so the next
/// meeting can start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wedged_close_frees_the_recorder_for_the_next_meeting() {
    let root = tmpdir("wedged-recorder");
    let released = Arc::new(AtomicBool::new(false));
    let for_opener = Arc::clone(&released);

    let rec = DaemonRecorder::with_parts(
        root.join("sessions"),
        tokio::runtime::Handle::current(),
        session::SegmentTap::default(),
        fotwd::recording::ReadyTap::default(),
        Box::new(move || {
            Ok((
                Box::new(NeverStoppingTap {
                    id: TapId::system_default(),
                    released: Arc::clone(&for_opener),
                }) as Box<dyn AudioTap>,
                None,
            ))
        }),
        Box::new(|| Transcription::Disabled),
        Box::new(|_root, _outcome| None),
        Duration::from_secs(3_600),
        Duration::from_secs(10),
    )
    .with_finish_deadlines(FinishDeadlines {
        close: Duration::from_millis(200),
        ..FinishDeadlines::default()
    });

    rec.start().expect("the tap starts cleanly");
    rec.stop().expect("stop trips the latch");

    let began = Instant::now();
    while rec.status().is_active() && began.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let state = rec.status().state;
    let waited = began.elapsed();
    released.store(true, Ordering::Relaxed);

    assert_eq!(
        state,
        RecordingState::Idle,
        "the recorder was still holding the slot {waited:?} after Stop — every \
         Start until the daemon restarts is refused with AlreadyRecording (#85)"
    );
    rec.start()
        .expect("a freed recorder must accept the next meeting");
    rec.stop().ok();
    let _ = std::fs::remove_dir_all(&root);
}

// ------------------------------------------------------------- a wedged pump

/// The decision this issue turned on, at the level where a human sees it.
///
/// The alternative was to abandon the pump thread and leave the directory for
/// `promote::resume`. It would never take it: `promote::pending` wants a
/// manifest with `ended_at_ms` *and* a `claim`, and an abandoned session has
/// neither — which is exactly the stranded directories #79 found. So the pump
/// stops itself and finalizes what it wrote: the meeting is short its tail,
/// not lost, and the tail is reported where degradation is reported (#79).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pump_that_gives_up_draining_reports_the_tail_it_dropped() {
    let root = tmpdir("relentless");
    let tap = RelentlessTap::new();
    let released = Arc::clone(&tap.released);
    let mut control = SessionControl::new();
    control.deadlines = FinishDeadlines {
        // Effectively "as soon as the pump notices". The rings are saturated
        // at that moment — `RelentlessTap::stop` does not answer until they
        // are — so the abandoned tail is guaranteed rather than hoped for,
        // and the fixture writes a megabyte rather than a gigabyte.
        drain: Duration::from_millis(1),
        ..FinishDeadlines::default()
    };
    let ready = control.ready.clone();
    let stop = control.stop.clone();

    let finished = spawn_session(&root, Box::new(tap), control);
    assert!(
        ready.wait_timeout(Duration::from_secs(10)),
        "capture never went live"
    );
    stop.stop();

    let ended = finished.recv_timeout(Duration::from_secs(20));
    released.store(true, Ordering::Relaxed);

    let outcome = ended
        .expect("a tap that kept delivering after stop() wedged the pump (#85)")
        .expect("an abandoned drain is a degraded meeting, not a failed one");

    assert!(
        outcome
            .stt_errors
            .iter()
            .any(|e| e.contains("drain deadline")),
        "the abandoned tail was not reported: {:?}",
        outcome.stt_errors
    );

    let manifest = std::fs::read_to_string(outcome.dir.join("manifest.json")).expect("a manifest");
    assert!(
        manifest.contains("ended_at_ms"),
        "the session was left unfinalized, which is precisely what \
         promote::pending skips forever (#79): {manifest}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The backstop. A pump stuck somewhere no latch reaches — a `write(2)` into a
/// filesystem that stopped answering — still gives the recorder back, and the
/// session says plainly that the directory it leaves behind is nobody's.
///
/// The deadline is zero because that is the only value that forces the case
/// without a fixture that can fake a hung disk: the pump is still inside
/// `IDLE_POLL` when the latch is set, so it cannot have answered, and the
/// expiry is the contract rather than a race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_join_deadline_ends_a_pump_that_has_not_come_back() {
    let root = tmpdir("join-deadline");
    let mut control = SessionControl::new();
    control.deadlines = FinishDeadlines {
        join: Duration::ZERO,
        ..FinishDeadlines::default()
    };
    let ready = control.ready.clone();
    let stop = control.stop.clone();

    let finished = spawn_session(&root, file_tap(30.0), control);
    assert!(
        ready.wait_timeout(Duration::from_secs(10)),
        "capture never went live"
    );
    stop.stop();

    let message = finished
        .recv_timeout(Duration::from_secs(20))
        .expect("the join deadline did not end the wait (#85)")
        .expect_err("a pump that has not come back is not a meeting");

    assert!(
        message.contains("ended_at_ms") && message.contains("claim"),
        "the error must say why the leftover directory is nobody's, or \
         \"leave it for the recovery path\" is a comfortable fiction: {message}"
    );
    // The pump is detached and finishing on its own; let it, so the cleanup
    // below is not racing a live writer.
    std::thread::sleep(Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(&root);
}

// ------------------------------------------------------------- no regression

/// The deadlines must be invisible to a healthy meeting: a stop that drains
/// cleanly abandons nothing, reports nothing, and waits for nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_healthy_stop_abandons_nothing() {
    let root = tmpdir("healthy");
    let control = SessionControl::new();
    let ready = control.ready.clone();
    let stop = control.stop.clone();

    let finished = spawn_session(&root, file_tap(30.0), control);
    assert!(
        ready.wait_timeout(Duration::from_secs(10)),
        "capture never went live"
    );
    // Long enough for the tap to have delivered something worth draining.
    std::thread::sleep(Duration::from_millis(300));

    let began = Instant::now();
    stop.stop();
    let outcome = finished
        .recv_timeout(Duration::from_secs(20))
        .expect("a healthy meeting must end")
        .expect("a healthy meeting must succeed");
    let waited = began.elapsed();

    assert!(
        waited < Duration::from_secs(5),
        "a healthy stop waited out a deadline it should never have reached: \
         {waited:?}"
    );
    assert!(
        outcome.captured_audio(),
        "the drain deadline ate a healthy meeting's audio"
    );
    assert!(
        outcome.stt_errors.is_empty(),
        "a clean stop must not report a degradation: {:?}",
        outcome.stt_errors
    );
    let _ = std::fs::remove_dir_all(&root);
}
