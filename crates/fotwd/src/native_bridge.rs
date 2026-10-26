//! Bounded command/status transport for the native main thread.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::recording::{RecorderObservation, StartVisibility, StopTarget};
use crate::session::StopSignal;

/// One renderer's lifetime and attempt-specific preparation acknowledgement.
#[derive(Clone)]
pub struct RenderGate(Arc<RenderInner>);
struct RenderInner {
    state: Mutex<RenderState>,
    changed: Condvar,
    deadline: Duration,
    closed: AtomicBool,
}
#[derive(Default)]
struct RenderState {
    pending: Option<StopTarget>,
    rendered: bool,
}
impl RenderGate {
    /// Bound how long startup may wait for an indicator frame.
    #[must_use]
    pub fn new(deadline: Duration) -> Self {
        Self(Arc::new(RenderInner {
            state: Mutex::new(RenderState::default()),
            changed: Condvar::new(),
            deadline,
            closed: AtomicBool::new(false),
        }))
    }
    /// Install on the recorder before exposing any Start control.
    #[must_use]
    pub fn admission(&self) -> StartVisibility {
        let gate = self.clone();
        StartVisibility::new(move |observation, stop| gate.wait(observation, &stop))
    }
    fn wait(&self, observation: RecorderObservation, stop: &StopSignal) -> Result<(), String> {
        let target = observation
            .stop_target()
            .cloned()
            .ok_or_else(|| "recording preparation no longer owns an attempt".to_owned())?;
        let began = Instant::now();
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending = Some(target);
        state.rendered = false;
        loop {
            let failure = if self.0.closed.load(Ordering::Acquire) {
                Some("native recording interface closed before capture")
            } else if stop.is_stopped() {
                Some("recording preparation cancelled before capture")
            } else if began.elapsed() >= self.0.deadline {
                Some("recording indicator did not render; capture was not started")
            } else {
                None
            };
            if let Some(failure) = failure {
                state.pending = None;
                return Err(failure.into());
            }
            if state.rendered {
                state.pending = None;
                return Ok(());
            }
            let remaining = self.0.deadline.saturating_sub(began.elapsed());
            (state, _) = self
                .0
                .changed
                .wait_timeout(state, remaining.min(Duration::from_millis(50)))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
    /// Call only after rendering the frame for this exact observed attempt.
    /// Returns false rather than waiting if the gate is currently busy.
    pub fn did_render(&self, target: &StopTarget) -> bool {
        let Ok(mut state) = self.0.state.try_lock() else {
            return false;
        };
        if self.0.closed.load(Ordering::Acquire)
            || !state
                .pending
                .as_ref()
                .is_some_and(|p| p.same_attempt(target))
        {
            return false;
        }
        state.rendered = true;
        self.0.changed.notify_all();
        true
    }
    /// Refuse new capture if the native surface goes away.
    pub fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.changed.notify_all();
    }
}

use crate::recording::DaemonRecorder;
use fotw_shell::StartOrigin;
use std::sync::mpsc::{self, Receiver, SyncSender};

/// Cancellation capability for one queued/native Start, never a successor.
#[derive(Clone)]
pub struct StartTicket {
    id: u64,
    stop: StopSignal,
}
impl StartTicket {
    /// Cancel this request, including while its configuration is blocked.
    pub fn cancel(&self) {
        self.stop.stop();
    }
    /// Correlate a completion without assuming it describes the current capture.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }
}
/// One Start completion. The observation remains authoritative for capture state.
#[derive(Clone, Debug)]
pub struct StartOutcome {
    /// The original Start request.
    pub id: u64,
    /// Actionable failure, if startup did not complete.
    pub error: Option<String>,
}
/// Latest worker observation. Only one snapshot is retained, irrespective of lag.
#[derive(Clone, Debug)]
pub struct BridgeSnapshot {
    /// Monotonic publication order within this bridge.
    pub sequence: u64,
    /// Atomic status and its matching Stop capability.
    pub observation: RecorderObservation,
    /// Last Start completion, correlated to its request.
    pub start_outcome: Option<StartOutcome>,
    /// Last command failure; it does not replace capture status.
    pub command_error: Option<String>,
}
enum Command {
    Start {
        origin: StartOrigin,
        ticket: StartTicket,
    },
    Stop(StopTarget),
}
/// The native thread only uses try-send, try-lock and cancellation atomics.
/// Recorder methods and Start waiting execute on background workers.
pub struct RecorderBridge {
    commands: SyncSender<Command>,
    latest: Arc<Mutex<Option<BridgeSnapshot>>>,
    busy: Arc<AtomicBool>,
    shutdown: StopSignal,
    gate: RenderGate,
    pending: Option<StartTicket>,
    next_id: u64,
}
impl RecorderBridge {
    /// Start a bounded bridge for the recorder sharing this render gate.
    ///
    /// # Errors
    /// If the operating system cannot create the background worker.
    pub fn spawn(recorder: Arc<DaemonRecorder>, gate: RenderGate) -> Result<Self, String> {
        let (commands, receiver) = mpsc::sync_channel(4);
        let latest = Arc::new(Mutex::new(None));
        let busy = Arc::new(AtomicBool::new(false));
        let shutdown = StopSignal::new();
        let worker_latest = latest.clone();
        let worker_busy = busy.clone();
        let worker_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("fotw-native-status".into())
            .spawn(move || {
                bridge_worker(
                    &recorder,
                    &receiver,
                    &worker_latest,
                    &worker_busy,
                    &worker_shutdown,
                )
            })
            .map_err(|_| "could not start native recording controls".to_owned())?;
        Ok(Self {
            commands,
            latest,
            busy,
            shutdown,
            gate,
            pending: None,
            next_id: 0,
        })
    }
    /// Submit explicitly acknowledged Start without waiting for configuration.
    ///
    /// # Errors
    /// Missing acknowledgement, another pending Start, or an unavailable queue.
    pub fn start(
        &mut self,
        origin: StartOrigin,
        acknowledged: bool,
    ) -> Result<StartTicket, String> {
        if !acknowledged {
            return Err("confirm that everyone consented before recording".into());
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("a recording start is already pending".into());
        }
        let Some(id) = self.next_id.checked_add(1) else {
            self.busy.store(false, Ordering::Release);
            return Err("restart the native recording interface".into());
        };
        let ticket = StartTicket {
            id,
            stop: StopSignal::new(),
        };
        if self
            .commands
            .try_send(Command::Start {
                origin,
                ticket: ticket.clone(),
            })
            .is_err()
        {
            self.busy.store(false, Ordering::Release);
            return Err("native recording controls are busy or unavailable; try again".into());
        }
        self.next_id = id;
        self.pending = Some(ticket.clone());
        Ok(ticket)
    }
    /// Submit Stop for the attempt actually displayed. Never retarget it later.
    ///
    /// # Errors
    /// If the bounded command queue is busy or unavailable.
    pub fn stop(&self, target: &StopTarget) -> Result<(), String> {
        self.commands
            .try_send(Command::Stop(target.clone()))
            .map_err(|_| "native Stop could not be queued; try again".into())
    }
    /// Take the latest snapshot without waiting on any worker or recorder lock.
    pub fn poll(&mut self) -> Option<BridgeSnapshot> {
        let snapshot = self.latest.try_lock().ok()?.take()?;
        if snapshot.start_outcome.as_ref().is_some_and(|outcome| {
            self.pending
                .as_ref()
                .is_some_and(|pending| pending.id == outcome.id)
        }) {
            self.pending = None;
        }
        Some(snapshot)
    }
    /// Acknowledge only after the matching preparation indicator was rendered.
    pub fn did_render(&self, target: &StopTarget) -> bool {
        self.gate.did_render(target)
    }
}
impl Drop for RecorderBridge {
    fn drop(&mut self) {
        self.gate.close();
        if let Some(pending) = &self.pending {
            pending.cancel();
        }
        self.shutdown.stop();
    }
}
fn bridge_worker(
    recorder: &Arc<DaemonRecorder>,
    commands: &Receiver<Command>,
    latest: &Mutex<Option<BridgeSnapshot>>,
    busy: &Arc<AtomicBool>,
    shutdown: &StopSignal,
) {
    let (finished, completions) = mpsc::sync_channel::<StartOutcome>(1);
    let mut outcome = None;
    let mut command_error = None;
    let mut sequence = 0_u64;
    let mut last_target: Option<StopTarget> = None;
    while !shutdown.is_stopped() {
        if let Ok(completion) = completions.try_recv() {
            outcome = Some(completion);
            busy.store(false, Ordering::Release);
        }
        // A bounded drain keeps status publication moving under repeated clicks.
        for _ in 0..4 {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            match command {
                Command::Start { origin, ticket } => {
                    outcome = None;
                    command_error = None;
                    let recorder = recorder.clone();
                    let finished = finished.clone();
                    let id = ticket.id;
                    if std::thread::Builder::new()
                        .name("fotw-native-start".into())
                        .spawn(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    recorder.start_native(origin, true, ticket.stop)
                                }));
                            let error = match result {
                                Ok(Ok(_)) => None,
                                Ok(Err(error)) => Some(error.to_string()),
                                Err(_) => Some("recording preparation failed; try again".into()),
                            };
                            let _ = finished.send(StartOutcome { id, error });
                        })
                        .is_err()
                    {
                        outcome = Some(StartOutcome {
                            id,
                            error: Some("could not start recording preparation".into()),
                        });
                        busy.store(false, Ordering::Release);
                    }
                }
                Command::Stop(target) => {
                    command_error = recorder.stop_observed(&target).err().map(|e| e.to_string());
                }
            }
        }
        sequence = sequence.saturating_add(1);
        let observation = recorder.observe();
        if observation.stop_target().is_some_and(|target| {
            !last_target
                .as_ref()
                .is_some_and(|previous| previous.same_attempt(target))
        }) {
            command_error = None;
        }
        last_target = observation.stop_target().cloned();
        let snapshot = BridgeSnapshot {
            sequence,
            observation,
            start_outcome: outcome.clone(),
            command_error: command_error.clone(),
        };
        *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot);
        std::thread::sleep(Duration::from_millis(25));
    }
    // Losing the only native surface must not leave invisible capture running.
    // This only requests Stop; native/session cleanup retains its normal owner.
    if let Some(target) = recorder.observe().stop_target() {
        let _ = recorder.stop_observed(target);
    }
}
