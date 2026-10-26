//! Bounded asynchronous AEC ownership, after original audio has been archived.
use crate::aec::{Adapter, Backend, BypassReason, Config as AdapterConfig, Output, OutputKind};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
};

/// Worker and owner retention limits; none of these are acoustic acceptance.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Fixed backend adapter shape and internal history budgets.
    pub adapter: AdapterConfig,
    /// Maximum queued input commands, from one through sixteen.
    pub queued_blocks: usize,
    /// Maximum samples per input command, at most 3200.
    pub block_samples: usize,
    /// Maximum original samples retained by the owner, at most 16000.
    pub pending_samples: usize,
    /// Oldest outstanding input expires after this many monotonic milliseconds.
    pub deadline_ms: u64,
}
/// Input is rejected before acceptance on these errors.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Invalid budgets/backend shape, or OS thread creation failed.
    Config,
    /// Closed stream, wrong route, discontinuous source or invalid PCM.
    Input,
    /// Owner time went backwards.
    Clock,
}
/// Metadata-only native resource lifetime; never retains a backend.
#[derive(Clone)]
pub struct Status(Arc<AtomicBool>);
impl Status {
    /// Remains true through native destruction, even after a caller deadline.
    pub fn is_running(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
struct Exit(Status);
impl Drop for Exit {
    fn drop(&mut self) {
        self.0.0.store(false, Ordering::Release);
    }
}
struct Input {
    fence: u64,
    start: u64,
    mic: Vec<f32>,
    reference: Option<Vec<f32>>,
}
enum Command {
    Input(Input),
    Finish,
}
struct ResultBlock {
    fence: u64,
    blocks: Vec<Output>,
}
struct Pending {
    start: u64,
    samples: Vec<f32>,
    used: usize,
    accepted_ms: u64,
}
/// Single-owner nonblocking command and publication boundary.
/// Call on the recording pump/control thread, never a realtime callback. The
/// caller must archive mic/system PCM before admission and persist processed
/// output with its own replay provenance. This object does neither disk IO nor
/// native reference-clock alignment. A reset retains exclusive ownership of the
/// same native worker; it never starts another backend beside a stalled one.
pub struct Worker {
    config: Config,
    epoch: u64,
    next: u64,
    fence: Arc<AtomicU64>,
    commands: Option<SyncSender<Command>>,
    results: Receiver<ResultBlock>,
    status: Status,
    pending: VecDeque<Pending>,
    retained: usize,
    now: u64,
    closed: bool,
}
impl Worker {
    /// Move a preloaded backend onto its exclusive worker thread.
    pub fn new<B: Backend + Send + 'static>(
        backend: B,
        config: Config,
        epoch: u64,
        start: u64,
    ) -> Result<Self, Error> {
        if !(1..=16).contains(&config.queued_blocks)
            || !(1..=3200).contains(&config.block_samples)
            || config.pending_samples < config.block_samples
            || config.pending_samples > 16000
            || !(1..=5000).contains(&config.deadline_ms)
        {
            return Err(Error::Config);
        }
        let adapter = Adapter::new(backend, config.adapter, 1, start).map_err(|_| Error::Config)?;
        let (commands, input) = mpsc::sync_channel(config.queued_blocks);
        let (output, results) = mpsc::sync_channel(config.queued_blocks + 1);
        let fence = Arc::new(AtomicU64::new(1));
        let worker_fence = fence.clone();
        let status = Status(Arc::new(AtomicBool::new(true)));
        let running = status.clone();
        std::thread::Builder::new()
            .name("fotw-aec".into())
            .spawn(move || {
                // Backend must drop before the observer reports resource release,
                // including during unwinding. Locals drop in reverse order.
                let _exit = Exit(running);
                let mut adapter = adapter;
                run(&mut adapter, input, output, &worker_fence);
            })
            .map_err(|_| Error::Config)?;
        Ok(Self {
            config,
            epoch,
            next: start,
            fence,
            commands: Some(commands),
            results,
            status,
            pending: VecDeque::with_capacity(config.pending_samples),
            retained: 0,
            now: 0,
            closed: false,
        })
    }
    /// Observe whether native work or destruction is still running.
    pub fn status(&self) -> Status {
        self.status.clone()
    }
    /// Retained original microphone samples, excluding bounded worker copies.
    pub fn pending_samples(&self) -> usize {
        self.retained
    }
    /// Submit only after archiving originals; reference must share this clock.
    /// Saturation is accepted as original-audio fallback, preserving source
    /// duration; invalid input is rejected without accepting that input.
    pub fn submit(
        &mut self,
        epoch: u64,
        start: u64,
        mic: &[f32],
        reference: Option<&[f32]>,
        now_ms: u64,
        out: &mut Vec<Output>,
    ) -> Result<(), Error> {
        if self.closed
            || epoch != self.epoch
            || start != self.next
            || mic.len() > self.config.block_samples
            || start.checked_add(mic.len() as u64).is_none()
            || mic.iter().any(|s| !s.is_finite())
            || reference.is_some_and(|r| r.len() != mic.len() || r.iter().any(|s| !s.is_finite()))
        {
            return Err(Error::Input);
        }
        self.poll(now_ms, out)?;
        if mic.is_empty() {
            return Ok(());
        }
        if self.retained + mic.len() > self.config.pending_samples {
            self.fallback(BypassReason::WorkerBusy, out);
        }
        self.next += mic.len() as u64;
        if self.commands.is_none() || !self.status.is_running() {
            out.push(Output {
                epoch: self.epoch,
                start_sample: start,
                samples: mic.to_vec(),
                kind: OutputKind::Raw(BypassReason::BackendFailed),
            });
            return Ok(());
        }
        let input = Input {
            fence: self.fence.load(Ordering::Acquire),
            start,
            mic: mic.to_vec(),
            reference: reference.map(<[f32]>::to_vec),
        };
        self.pending.push_back(Pending {
            start,
            samples: mic.to_vec(),
            used: 0,
            accepted_ms: now_ms,
        });
        self.retained += mic.len();
        match self
            .commands
            .as_ref()
            .unwrap()
            .try_send(Command::Input(input))
        {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.fallback(BypassReason::WorkerBusy, out),
            Err(TrySendError::Disconnected(_)) => {
                self.fallback(BypassReason::BackendFailed, out);
                self.commands.take();
            }
        }
        Ok(())
    }
    /// Drain currently available results and enforce the owner deadline.
    pub fn poll(&mut self, now_ms: u64, out: &mut Vec<Output>) -> Result<(), Error> {
        if now_ms < self.now {
            return Err(Error::Clock);
        }
        self.now = now_ms;
        // Deadline precedes result acceptance: a result waiting in the mailbox
        // cannot bypass an expired original's publication deadline.
        if self
            .pending
            .front()
            .is_some_and(|p| now_ms - p.accepted_ms >= self.config.deadline_ms)
        {
            self.fallback(BypassReason::WorkerDeadline, out);
        }
        // Fixed per-poll work even if a producer keeps publishing concurrently.
        for _ in 0..self.config.queued_blocks + 1 {
            match self.results.try_recv() {
                Ok(result) => {
                    if result.fence != self.fence.load(Ordering::Acquire) {
                        continue;
                    }
                    for mut block in result.blocks {
                        if !self.accept(&block) {
                            self.fallback(BypassReason::BackendFailed, out);
                            self.commands.take();
                            break;
                        }
                        block.epoch = self.epoch;
                        out.push(block);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.fallback(BypassReason::BackendFailed, out);
                    self.commands.take();
                    break;
                }
            }
        }
        Ok(())
    }
    /// Fence the old route and publish its remaining original audio immediately.
    pub fn reset(&mut self, epoch: u64, start: u64, out: &mut Vec<Output>) -> Result<(), Error> {
        if self.closed || epoch <= self.epoch {
            return Err(Error::Input);
        }
        self.fallback(BypassReason::Reset, out);
        self.epoch = epoch;
        self.next = start;
        Ok(())
    }
    /// Request tail processing without waiting for native work.
    /// Caller continues polling until originals are published or their deadline
    /// expires. Resource status may remain running after publication completes.
    pub fn finish(&mut self, now_ms: u64, out: &mut Vec<Output>) -> Result<(), Error> {
        self.poll(now_ms, out)?;
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if let Some(sender) = self.commands.take()
            && sender.try_send(Command::Finish).is_err()
        {
            self.fallback(BypassReason::WorkerBusy, out);
        }
        Ok(())
    }
    /// Publish all remaining originals and fence native work immediately. Does
    /// not wait for a blocked call or destructor; observe `status()` separately.
    pub fn cancel(&mut self, out: &mut Vec<Output>) {
        self.fallback(BypassReason::WorkerCancelled, out);
        self.closed = true;
        self.commands.take();
    }
    fn fallback(&mut self, reason: BypassReason, out: &mut Vec<Output>) {
        // Saturation/timeout/reset invalidate every in-flight result, including
        // native calls that cannot be cancelled. Never reuse a wrapped fence.
        let old = self.fence.load(Ordering::Acquire);
        if let Some(next) = old.checked_add(1) {
            self.fence.store(next, Ordering::Release);
        } else {
            // Zero is never an admitted generation. A terminal marker also
            // releases publication waiting at the exhausted fence.
            self.fence.store(0, Ordering::Release);
            self.commands.take();
            self.closed = true;
        }
        for p in self.pending.drain(..) {
            out.push(Output {
                epoch: self.epoch,
                start_sample: p.start + p.used as u64,
                samples: p.samples[p.used..].to_vec(),
                kind: OutputKind::Raw(reason),
            });
        }
        self.retained = 0;
    }
    fn accept(&mut self, block: &Output) -> bool {
        let Some(front) = self.pending.front() else {
            return false;
        };
        if block.start_sample != front.start + front.used as u64
            || block.samples.len() > self.retained
            || block.samples.is_empty()
        {
            return false;
        }
        let mut remaining = block.samples.len();
        while remaining > 0 {
            let front = self.pending.front_mut().unwrap();
            let n = remaining.min(front.samples.len() - front.used);
            front.used += n;
            remaining -= n;
            self.retained -= n;
            if front.used == front.samples.len() {
                self.pending.pop_front();
            }
        }
        true
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self
            .fence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1));
        self.commands.take();
    }
}
fn run<B: Backend>(
    adapter: &mut Adapter<B>,
    input: Receiver<Command>,
    output: SyncSender<ResultBlock>,
    fence: &AtomicU64,
) {
    let mut active = 1;
    while let Ok(command) = input.recv() {
        let mut blocks = Vec::new();
        let current = fence.load(Ordering::Acquire);
        match command {
            Command::Input(job) => {
                if job.fence != current {
                    continue;
                }
                if active != current {
                    if adapter.reset(current, job.start, &mut blocks).is_err() {
                        return;
                    }
                    blocks.clear(); // Owner already released all prior originals.
                    active = current;
                }
                if let Some(reference) = job.reference.as_ref()
                    && adapter
                        .push_render(active, job.start, reference, &mut blocks)
                        .is_err()
                {
                    return;
                }
                if adapter
                    .push_capture(active, job.start, &job.mic, &mut blocks)
                    .is_err()
                {
                    return;
                }
                if job.reference.is_none() {
                    adapter.flush_reference_wait(&mut blocks);
                }
            }
            Command::Finish => {
                if active == current {
                    adapter.finish(&mut blocks);
                }
            }
        }
        if !blocks.is_empty()
            && !publish(
                &output,
                fence,
                ResultBlock {
                    fence: active,
                    blocks,
                },
            )
        {
            return;
        }
    }
}

fn publish(output: &SyncSender<ResultBlock>, fence: &AtomicU64, mut result: ResultBlock) -> bool {
    loop {
        // The owner can release originals while native work is finishing.
        // Cancellation/reset/drop must also release a full result mailbox.
        if result.fence != fence.load(Ordering::Acquire) {
            return true;
        }
        match output.try_send(result) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(pending)) => {
                // Backpressure stays on this exclusive worker. No new native
                // call, allocation or owner/callback wait is introduced.
                result = pending;
                std::thread::park_timeout(std::time::Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod result_backpressure_tests {
    use super::*;
    use std::{thread::JoinHandle, time::Duration};

    struct Identity {
        calls: mpsc::Sender<usize>,
        count: usize,
    }
    impl Backend for Identity {
        type Error = ();
        fn frame_samples(&self) -> usize {
            160
        }
        fn delay_samples(&self) -> usize {
            0
        }
        fn analyze_render(&mut self, _: &[f32]) -> Result<(), ()> {
            Ok(())
        }
        fn process_capture(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ()> {
            output.copy_from_slice(input);
            self.count += 1;
            self.calls.send(self.count).unwrap();
            Ok(())
        }
        fn reset(&mut self) -> Result<(), ()> {
            Ok(())
        }
    }

    struct PausedOwner {
        input: SyncSender<Command>,
        output: Receiver<ResultBlock>,
        calls: Receiver<usize>,
        fence: Arc<AtomicU64>,
        done: Receiver<()>,
        thread: JoinHandle<()>,
    }
    fn job(index: u64) -> Command {
        Command::Input(Input {
            fence: 1,
            start: index * 160,
            mic: vec![0.5; 160],
            reference: Some(vec![0.1; 160]),
        })
    }
    fn paused_owner() -> PausedOwner {
        // Match a production queue of four commands and five results. Fill the
        // result mailbox while an already-admitted sixth job completes.
        let (input, jobs) = mpsc::sync_channel(4);
        let (results, output) = mpsc::sync_channel(5);
        let (progress, calls) = mpsc::channel();
        let (completed, done) = mpsc::channel();
        let fence = Arc::new(AtomicU64::new(1));
        let worker_fence = fence.clone();
        let thread = std::thread::spawn(move || {
            let mut adapter = Adapter::new(
                Identity {
                    calls: progress,
                    count: 0,
                },
                AdapterConfig {
                    frame_samples: 160,
                    delay_samples: 0,
                    max_wait_samples: 320,
                    max_reference_samples: 640,
                },
                1,
                0,
            )
            .unwrap();
            run(&mut adapter, jobs, results, &worker_fence);
            drop(adapter);
            completed.send(()).unwrap();
        });
        for index in 0..6 {
            input.send(job(index)).unwrap();
            assert_eq!(
                calls.recv_timeout(Duration::from_secs(2)).unwrap(),
                index as usize + 1
            );
        }
        PausedOwner {
            input,
            output,
            calls,
            fence,
            done,
            thread,
        }
    }

    #[test]
    fn full_result_mailbox_recovers_when_the_owner_resumes_polling() {
        let owner = paused_owner();
        assert_eq!(
            owner.calls.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "a full result mailbox must retain the live backend, not disconnect it"
        );
        owner.input.send(job(6)).unwrap();
        let first = owner.output.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first.blocks[0].start_sample, 0);
        assert_eq!(first.blocks[0].samples, vec![0.5; 160]);
        assert_eq!(owner.calls.recv_timeout(Duration::from_secs(2)).unwrap(), 7);
        for index in 1..7 {
            let result = owner.output.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(result.fence, 1);
            assert_eq!(result.blocks.len(), 1);
            assert_eq!(result.blocks[0].start_sample, index * 160);
            assert_eq!(result.blocks[0].samples, vec![0.5; 160]);
        }
        drop(owner.output);
        drop(owner.input);
        owner.done.recv_timeout(Duration::from_secs(2)).unwrap();
        owner.thread.join().unwrap();
    }

    #[test]
    fn fencing_a_full_result_mailbox_releases_the_worker_without_owner_polling() {
        let owner = paused_owner();
        owner.fence.store(2, Ordering::Release);
        drop(owner.input);
        // Keep the result receiver alive and full: cancellation must be enough.
        owner.done.recv_timeout(Duration::from_secs(2)).unwrap();
        owner.thread.join().unwrap();
        drop(owner.output);
    }

    #[test]
    fn cancel_at_exhausted_fence_releases_a_full_mailbox_without_owner_polling() {
        let (progress, calls) = mpsc::channel();
        let mut worker = Worker::new(
            Identity {
                calls: progress,
                count: 0,
            },
            Config {
                adapter: AdapterConfig {
                    frame_samples: 160,
                    delay_samples: 0,
                    max_wait_samples: 320,
                    max_reference_samples: 640,
                },
                queued_blocks: 4,
                block_samples: 160,
                pending_samples: 16000,
                deadline_ms: 500,
            },
            1,
            0,
        )
        .unwrap();
        worker.fence.store(u64::MAX, Ordering::Release);
        // Exercise the native mailbox directly so owner polling cannot free it.
        for index in 0..6 {
            let Command::Input(mut input) = job(index) else {
                unreachable!()
            };
            input.fence = u64::MAX;
            worker
                .commands
                .as_ref()
                .unwrap()
                .send(Command::Input(input))
                .unwrap();
            assert_eq!(
                calls.recv_timeout(Duration::from_secs(2)).unwrap(),
                index as usize + 1
            );
        }
        worker.cancel(&mut Vec::new());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while worker.status().is_running() {
            assert!(
                std::time::Instant::now() < deadline,
                "cancel must release publication even when the fence cannot increment"
            );
            std::thread::yield_now();
        }
        assert!(worker.closed);
        assert_eq!(worker.fence.load(Ordering::Acquire), 0);
    }
}
