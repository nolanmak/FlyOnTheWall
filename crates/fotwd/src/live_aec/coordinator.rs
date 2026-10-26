//! Bounded recording-pump owner for qualified reference alignment and AEC work.
use super::SourceClock;
use fotw_pipeline::{
    aec::{Backend, BypassReason, Output, OutputKind},
    aec_worker::{Config as WorkerConfig, Worker},
    reference_resample::{
        AlignmentEpoch, Config as ReferenceConfig, HostNs, ProcessingFrame, ReferenceBuffer,
        Request,
    },
    ring::CaptureObservation,
};
use std::collections::VecDeque;

struct Pending {
    start: u64,
    samples: Vec<f32>,
    accepted_ms: u64,
}
struct NativeMono {
    channels: usize,
    next: Option<u64>,
    partial: Vec<f32>,
}
impl NativeMono {
    fn push(&mut self, start: u64, input: &[f32]) -> Option<(u64, Vec<f32>)> {
        if input.len() > 48000 || input.iter().any(|s| !s.is_finite()) {
            self.partial.clear();
            self.next = None;
            return None;
        }
        if self.next.is_some_and(|next| next != start) {
            self.partial.clear();
        }
        self.next = start.checked_add(input.len() as u64);
        let begin = start.checked_sub(self.partial.len() as u64)?;
        // Skip an unknown leading partial frame after an input discontinuity.
        let skip = (self.channels - begin as usize % self.channels) % self.channels;
        let mut joined = std::mem::take(&mut self.partial);
        joined.extend_from_slice(input);
        if skip > joined.len() {
            return None;
        }
        let complete = (joined.len() - skip) / self.channels * self.channels;
        let mono = joined[skip..skip + complete]
            .chunks_exact(self.channels)
            .map(|frame| {
                (frame.iter().map(|&s| f64::from(s)).sum::<f64>() / self.channels as f64) as f32
            })
            .collect();
        self.partial = joined[skip + complete..].to_vec();
        Some((begin.checked_add(skip as u64)?, mono))
    }
}

/// Recording-side owner. Call only after original capture has been archived;
/// this object performs neither disk writes nor realtime callback work.
pub struct Coordinator {
    worker: Worker,
    next: u64,
    epoch: u64,
    reference: ReferenceBuffer,
    config: ReferenceConfig,
    render: SourceClock,
    mic: SourceClock,
    mono: NativeMono,
    waiting: VecDeque<Pending>,
    waiting_samples: usize,
    block_samples: usize,
    now: u64,
    closed: bool,
}
impl Coordinator {
    /// Move a preloaded backend onto its exclusive worker and preallocate clocks.
    pub fn new<B: Backend + Send + 'static>(
        backend: B,
        worker: WorkerConfig,
        reference: ReferenceConfig,
        channels: (u16, u16),
        first_sample: u64,
    ) -> Result<Self, String> {
        let loaded = Worker::new(backend, worker, reference.epoch.0, first_sample)
            .map_err(|_| "invalid worker")?;
        Self::from_worker(loaded, worker, reference, channels, first_sample)
    }
    pub(crate) fn bind_prepared(
        mut loaded: Worker,
        worker: WorkerConfig,
        reference: ReferenceConfig,
        channels: (u16, u16),
        first_sample: u64,
    ) -> Result<Self, String> {
        let mut empty = Vec::new();
        loaded
            .reset(reference.epoch.0, first_sample, &mut empty)
            .map_err(|_| "processing admission could not bind its source")?;
        if !empty.is_empty() {
            return Err("processing admission contains prior recording input".into());
        }
        Self::from_worker(loaded, worker, reference, channels, first_sample)
    }
    fn from_worker(
        loaded: Worker,
        worker: WorkerConfig,
        reference: ReferenceConfig,
        channels: (u16, u16),
        first_sample: u64,
    ) -> Result<Self, String> {
        if !(1..=32).contains(&channels.0)
            || !(1..=32).contains(&channels.1)
            || worker.block_samples < 160
        {
            return Err("unsupported capture/processing shape".into());
        }
        let history = ReferenceBuffer::new(reference).map_err(|_| "invalid reference geometry")?;
        let render = SourceClock::new(
            reference.render_rate_hz,
            channels.0,
            reference.render_generation.0,
        )
        .map_err(|_| "invalid render clock")?;
        let mic = SourceClock::new(
            reference.mic.native_rate_hz,
            channels.1,
            reference.mic_generation.0,
        )
        .map_err(|_| "invalid microphone clock")?;
        Ok(Self {
            worker: loaded,
            next: first_sample,
            epoch: reference.epoch.0,
            reference: history,
            config: reference,
            render,
            mic,
            mono: NativeMono {
                channels: channels.0 as usize,
                next: None,
                partial: Vec::new(),
            },
            waiting: VecDeque::new(),
            waiting_samples: 0,
            block_samples: worker.block_samples,
            now: 0,
            closed: false,
        })
    }
    /// Observe the render source without treating its latest stamp as drain time.
    pub fn observe_render(
        &mut self,
        observation: CaptureObservation,
        now: u64,
        out: &mut Vec<Output>,
    ) -> Result<(), String> {
        self.advance(now)?;
        if self.render.observe(observation, now).is_err() {
            self.invalidate(out)?;
        }
        Ok(())
    }
    /// Observe the microphone source on the same capture-host clock.
    pub fn observe_mic(
        &mut self,
        observation: CaptureObservation,
        now: u64,
        out: &mut Vec<Output>,
    ) -> Result<(), String> {
        self.advance(now)?;
        if self.mic.observe(observation, now).is_err() {
            self.invalidate(out)?;
        }
        Ok(())
    }
    /// Append native interleaved render PCM after archiving its original capture.
    pub fn append_render(
        &mut self,
        accepted_start: u64,
        pcm: &[f32],
        now: u64,
        out: &mut Vec<Output>,
    ) -> Result<(), String> {
        self.advance(now)?;
        if let Some((start, mono)) = self.mono.push(accepted_start, pcm)
            && !mono.is_empty()
            && let Some(native) =
                self.render
                    .native_start(start, mono.len() * self.mono.channels, now)
        {
            for (i, chunk) in mono.chunks(self.config.render_rate_hz as usize).enumerate() {
                let start = fotw_pipeline::reference_resample::NativeFrame(
                    native
                        .0
                        .checked_add(i as u64 * u64::from(self.config.render_rate_hz))
                        .ok_or("native reference overflow")?,
                );
                if self
                    .reference
                    .append(
                        AlignmentEpoch(self.epoch),
                        self.config.render_generation,
                        start,
                        chunk,
                    )
                    .is_err()
                {
                    self.invalidate(out)?;
                    self.reference
                        .append(
                            AlignmentEpoch(self.epoch),
                            self.config.render_generation,
                            start,
                            chunk,
                        )
                        .map_err(|_| "reference input rejected")?;
                }
            }
        }
        self.poll(now, out)
    }
    /// Accept consecutive microphone PCM on its absolute canonical source clock.
    pub fn push_mic(
        &mut self,
        start: u64,
        pcm: &[f32],
        now: u64,
        out: &mut Vec<Output>,
    ) -> Result<(), String> {
        self.advance(now)?;
        if self.closed
            || start != self.next
            || pcm.iter().any(|s| !s.is_finite())
            || start.checked_add(pcm.len() as u64).is_none()
        {
            return Err("invalid microphone input".into());
        }
        for chunk in pcm.chunks(self.block_samples) {
            if self.waiting_samples + chunk.len() > 6400 {
                self.drain(true, out)?;
            }
            self.waiting.push_back(Pending {
                start: self.next,
                samples: chunk.to_vec(),
                accepted_ms: now / 1_000_000,
            });
            self.waiting_samples += chunk.len();
            self.next += chunk.len() as u64;
            self.poll(now, out)?;
        }
        Ok(())
    }
    /// Poll bounded results and original-audio deadlines without waiting on native calls.
    pub fn poll(&mut self, now: u64, out: &mut Vec<Output>) -> Result<(), String> {
        self.advance(now)?;
        self.worker
            .poll(now / 1_000_000, out)
            .map_err(|_| "AEC worker poll rejected")?;
        self.drain(self.closed, out)
    }
    /// Request final tail publication; continue polling until drained or cancel.
    pub fn finish(&mut self, now: u64, out: &mut Vec<Output>) -> Result<(), String> {
        self.advance(now)?;
        if !self.closed {
            self.drain(true, out)?;
            self.closed = true;
            self.worker
                .finish(now / 1_000_000, out)
                .map_err(|_| "AEC worker finish rejected")?;
        }
        Ok(())
    }
    /// Whether every accepted original has been published, independently of cleanup.
    pub fn is_drained(&self) -> bool {
        self.waiting.is_empty() && self.worker.pending_samples() == 0
    }
    /// Retain original pending samples and stop publication without native waiting.
    pub fn cancel(&mut self, out: &mut Vec<Output>) {
        self.worker.cancel(out);
        self.release_waiting(BypassReason::WorkerCancelled, out);
        self.closed = true;
    }
    /// Observe native lifetime through teardown, independently of data publication.
    pub fn status(&self) -> fotw_pipeline::aec_worker::Status {
        self.worker.status()
    }
    pub(crate) fn route_epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn next_sample(&self) -> u64 {
        self.next
    }
    pub(crate) fn reset_timing(&mut self, out: &mut Vec<Output>) -> Result<(), String> {
        self.invalidate(out)?;
        self.render = SourceClock::new(
            self.config.render_rate_hz,
            self.render.channels,
            self.config.render_generation.0,
        )
        .map_err(|_| "invalid render clock")?;
        self.mic = SourceClock::new(
            self.config.mic.native_rate_hz,
            self.mic.channels,
            self.config.mic_generation.0,
        )
        .map_err(|_| "invalid microphone clock")?;
        self.mono.partial.clear();
        self.mono.next = None;
        Ok(())
    }
    /// Replace route geometry without creating a second native worker. All
    /// earlier pending originals publish before the new epoch accepts output.
    /// Allocate this state on the pump/control thread, never a capture callback.
    pub fn reconfigure(
        &mut self,
        reference: ReferenceConfig,
        channels: (u16, u16),
        out: &mut Vec<Output>,
    ) -> Result<(), String> {
        if self.closed
            || reference.epoch.0 <= self.epoch
            || reference.render_generation.0 < self.config.render_generation.0
            || reference.mic_generation.0 < self.config.mic_generation.0
            || !(1..=32).contains(&channels.0)
            || !(1..=32).contains(&channels.1)
        {
            return Err("stale or invalid capture route".into());
        }
        let history = ReferenceBuffer::new(reference).map_err(|_| "invalid reference geometry")?;
        let render = SourceClock::new(
            reference.render_rate_hz,
            channels.0,
            reference.render_generation.0,
        )
        .map_err(|_| "invalid render clock")?;
        let mic = SourceClock::new(
            reference.mic.native_rate_hz,
            channels.1,
            reference.mic_generation.0,
        )
        .map_err(|_| "invalid microphone clock")?;
        self.worker
            .reset(reference.epoch.0, self.next, out)
            .map_err(|_| "AEC route reset rejected")?;
        self.release_waiting(BypassReason::Reset, out);
        self.reference = history;
        self.config = reference;
        self.epoch = reference.epoch.0;
        self.render = render;
        self.mic = mic;
        self.mono = NativeMono {
            channels: channels.0 as usize,
            next: None,
            partial: Vec::new(),
        };
        Ok(())
    }
    fn advance(&mut self, now: u64) -> Result<(), String> {
        if now < self.now {
            return Err("capture-host clock moved backwards".into());
        }
        self.now = now;
        Ok(())
    }
    fn drain(&mut self, force: bool, out: &mut Vec<Output>) -> Result<(), String> {
        while let Some(front) = self.waiting.front() {
            let mut reference = vec![0.0; front.samples.len()];
            let aligned = self
                .render
                .estimate(self.now)
                .zip(self.mic.estimate(self.now))
                .is_some_and(|(render, mic)| {
                    self.reference
                        .fill(
                            Request {
                                epoch: AlignmentEpoch(self.epoch),
                                start: ProcessingFrame(front.start),
                                now: HostNs(self.now),
                                render,
                                mic,
                            },
                            &mut reference,
                        )
                        .is_ok()
                });
            if !aligned && !force && self.now / 1_000_000 - front.accepted_ms < 200 {
                break;
            }
            let pending = self.waiting.pop_front().unwrap();
            if self
                .worker
                .submit(
                    self.epoch,
                    pending.start,
                    &pending.samples,
                    aligned.then_some(reference.as_slice()),
                    self.now / 1_000_000,
                    out,
                )
                .is_err()
            {
                self.waiting.push_front(pending);
                return Err("AEC worker input rejected".into());
            }
            self.waiting_samples -= pending.samples.len();
        }
        Ok(())
    }
    fn release_waiting(&mut self, reason: BypassReason, out: &mut Vec<Output>) {
        for pending in self.waiting.drain(..) {
            out.push(Output {
                epoch: self.epoch,
                start_sample: pending.start,
                samples: pending.samples,
                kind: OutputKind::Raw(reason),
            });
        }
        self.waiting_samples = 0;
    }
    fn invalidate(&mut self, out: &mut Vec<Output>) -> Result<(), String> {
        let next = self.epoch.checked_add(1).ok_or("AEC epoch overflow")?;
        self.worker
            .reset(next, self.next, out)
            .map_err(|_| "AEC reset rejected")?;
        self.release_waiting(BypassReason::Reset, out);
        self.reference
            .reset(
                AlignmentEpoch(next),
                self.config.render_generation,
                self.config.mic_generation,
                self.config.mic,
            )
            .map_err(|_| "AEC reference reset rejected")?;
        self.epoch = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_pipeline::{
        aec::Config,
        reference_resample::{
            AlignmentEpoch, FrameDomain, NativeFrame, ProcessingFrame, TapGeneration,
        },
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Subtract {
        render: Vec<f32>,
        calls: Arc<AtomicUsize>,
    }
    impl Backend for Subtract {
        type Error = ();
        fn frame_samples(&self) -> usize {
            160
        }
        fn delay_samples(&self) -> usize {
            0
        }
        fn analyze_render(&mut self, frame: &[f32]) -> Result<(), ()> {
            self.render = frame.to_vec();
            Ok(())
        }
        fn process_capture(&mut self, frame: &[f32], out: &mut [f32]) -> Result<(), ()> {
            assert_eq!(self.render.len(), 160, "render must be analyzed first");
            for ((sample, mic), render) in out.iter_mut().zip(frame).zip(&self.render) {
                *sample = mic - render;
            }
            self.render.clear();
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
        fn reset(&mut self) -> Result<(), ()> {
            self.render.clear();
            Ok(())
        }
    }
    fn observation(index: u64) -> CaptureObservation {
        CaptureObservation {
            sequence: index + 1,
            sample_start: index * 160,
            sample_count: 160,
            device_frames: index * 160,
            host_ns: 1_000_000_000 + index * 10_000_000,
            flags: 0,
            dropped_samples: 0,
            timing_faults: 0,
        }
    }
    fn config() -> (WorkerConfig, ReferenceConfig) {
        (
            WorkerConfig {
                adapter: Config {
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
            ReferenceConfig {
                render_rate_hz: 16000,
                capacity_frames: 32000,
                mic: FrameDomain {
                    native_origin: NativeFrame(0),
                    native_offset_frames: 0.0,
                    processing_origin: ProcessingFrame(2192),
                    native_rate_hz: 16000,
                },
                render_generation: TapGeneration(1),
                mic_generation: TapGeneration(1),
                epoch: AlignmentEpoch(1),
            },
        )
    }
    #[test]
    fn live_clocked_owner_processes_overlap_and_keeps_every_source_sample() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (worker, reference) = config();
        let mut owner = Coordinator::new(
            Subtract {
                render: Vec::new(),
                calls: calls.clone(),
            },
            worker,
            reference,
            (1, 1),
            2192,
        )
        .unwrap();
        let mut output = Vec::new();
        for i in 0..350 {
            let o = observation(i);
            let now = o.host_ns + 10_000_000;
            owner.observe_render(o, now, &mut output).unwrap();
            owner.observe_mic(o, now, &mut output).unwrap();
            let far: Vec<_> = (0..160)
                .map(|n| {
                    0.2 * ((i * 160 + n) as f32 * std::f32::consts::TAU * 500.0 / 16000.0).sin()
                })
                .collect();
            let mic: Vec<_> = far.iter().map(|f| f + 0.05).collect();
            owner
                .append_render(i * 160, &far, now, &mut output)
                .unwrap();
            owner
                .push_mic(2192 + i * 160, &mic, now, &mut output)
                .unwrap();
            // Let the fake worker settle without advancing the synthetic host
            // clock or allowing a late result to evade its publication deadline.
            let until = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while owner.worker.pending_samples() > 0 && std::time::Instant::now() < until {
                owner.poll(now, &mut output).unwrap();
                std::thread::yield_now();
            }
        }
        let now = 4_600_000_000;
        owner.finish(now, &mut output).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !owner.is_drained() {
            assert!(std::time::Instant::now() < until);
            owner.poll(now, &mut output).unwrap();
            std::thread::yield_now();
        }
        assert!(
            calls.load(Ordering::Acquire) > 100,
            "a healthy qualified stream must reach the backend"
        );
        let mut expected = 2192;
        let mut cleaned = 0;
        for block in output {
            assert_eq!(block.start_sample, expected);
            expected += block.samples.len() as u64;
            if block.kind == OutputKind::Cleaned {
                cleaned += block.samples.len();
                assert!(block.samples.iter().all(|s| (*s - 0.05).abs() < 0.0001));
            }
        }
        assert_eq!(expected, 2192 + 350 * 160);
        assert!(cleaned > 16000);
    }

    fn owner() -> Coordinator {
        let (worker, reference) = config();
        Coordinator::new(
            Subtract {
                render: Vec::new(),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            worker,
            reference,
            (1, 1),
            2192,
        )
        .unwrap()
    }
    fn finish(owner: &mut Coordinator, now: u64, output: &mut Vec<Output>) {
        owner.finish(now, output).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !owner.is_drained() {
            assert!(std::time::Instant::now() < until);
            owner.poll(now, output).unwrap();
            std::thread::yield_now();
        }
    }
    #[test]
    fn unavailable_reference_and_a_partial_stop_tail_preserve_exact_originals() {
        let mut owner = owner();
        let mut output = Vec::new();
        let input: Vec<_> = (0..3217).map(|i| (i as f32 * 0.0123).sin() * 0.2).collect();
        owner
            .push_mic(2192, &input, 1_000_000_000, &mut output)
            .unwrap();
        finish(&mut owner, 1_010_000_000, &mut output);
        let mut expected = 2192;
        let mut actual = Vec::new();
        for block in output {
            assert_eq!(block.start_sample, expected);
            expected += block.samples.len() as u64;
            assert!(matches!(block.kind, OutputKind::Raw(_)));
            actual.extend(block.samples);
        }
        assert_eq!(actual, input);
    }

    #[test]
    fn route_reset_publishes_old_pending_audio_and_fences_old_tap_positions() {
        let mut owner = owner();
        let mut output = Vec::new();
        let input = vec![0.123; 317];
        owner
            .push_mic(2192, &input, 1_000_000_000, &mut output)
            .unwrap();
        let (_, mut route) = config();
        route.epoch = AlignmentEpoch(2);
        route.mic_generation = TapGeneration(2);
        route.mic.native_rate_hz = 44100;
        route.mic.processing_origin = ProcessingFrame(2509);
        owner.reconfigure(route, (1, 2), &mut output).unwrap();
        assert_eq!(
            output
                .iter()
                .flat_map(|o| o.samples.iter())
                .copied()
                .collect::<Vec<_>>(),
            input
        );
        assert!(
            output
                .iter()
                .all(|o| o.epoch == 1 && o.kind == OutputKind::Raw(BypassReason::Reset))
        );
        assert!(
            owner.reconfigure(route, (1, 2), &mut output).is_err(),
            "a route cannot replay its epoch"
        );
        owner
            .push_mic(2509, &[0.23; 19], 1_010_000_000, &mut output)
            .unwrap();
        finish(&mut owner, 1_020_000_000, &mut output);
        let last = output.last().unwrap();
        assert_eq!(last.start_sample, 2509);
        assert_eq!(last.epoch, 2);
        assert_eq!(last.samples, [0.23; 19]);
    }

    #[test]
    fn render_partial_frames_and_large_finite_values_do_not_corrupt_mono_history() {
        let mut mono = NativeMono {
            channels: 3,
            next: None,
            partial: Vec::new(),
        };
        let (start, first) = mono.push(0, &[0.1, 0.2]).unwrap();
        assert_eq!(start, 0);
        assert!(first.is_empty());
        let (start, next) = mono.push(2, &[0.3, 0.4, 0.5, 0.6]).unwrap();
        assert_eq!(start, 0);
        assert_eq!(next.len(), 2);
        assert!((next[0] - 0.2).abs() < 1e-7);
        assert!((next[1] - 0.5).abs() < 1e-7);
        let (_, large) = mono.push(6, &[f32::MAX; 3]).unwrap();
        assert_eq!(large, [f32::MAX]);
        assert!(mono.partial.len() < mono.channels);
    }

    #[test]
    fn invalid_input_and_backwards_time_do_not_consume_microphone_positions() {
        let mut owner = owner();
        let mut output = Vec::new();
        assert!(
            owner
                .push_mic(2192, &[f32::NAN], 1_000_000_000, &mut output)
                .is_err()
        );
        assert!(
            owner
                .push_mic(2191, &[0.1], 1_000_000_000, &mut output)
                .is_err()
        );
        assert!(
            owner
                .push_mic(2192, &[0.1], 999_999_999, &mut output)
                .is_err()
        );
        assert_eq!(owner.next, 2192);
        assert!(output.is_empty());
        owner
            .push_mic(2192, &[0.1], 1_000_000_000, &mut output)
            .unwrap();
        finish(&mut owner, 1_010_000_000, &mut output);
        assert_eq!(
            output
                .iter()
                .flat_map(|o| o.samples.iter())
                .copied()
                .collect::<Vec<_>>(),
            [0.1]
        );
    }

    #[test]
    fn microphone_ring_loss_relearns_the_accepted_sample_clock_before_processing_resumes() {
        ring_loss(false);
    }
    #[test]
    fn render_ring_loss_does_not_confuse_device_and_accepted_reference_frames() {
        ring_loss(true);
    }
    fn ring_loss(render_loses: bool) {
        let mut owner = owner();
        let mut output = Vec::new();
        let wave = |frame: u64| {
            (0.2 * (frame as f64 * std::f64::consts::TAU * 617.0 / 16000.0).sin()) as f32
        };
        for i in 0..550 {
            let mut render = observation(i);
            let mut mic = render;
            if i >= 230 {
                let affected = if render_loses { &mut render } else { &mut mic };
                affected.device_frames += 80;
                affected.dropped_samples = 80;
                affected.host_ns += 5_000_000;
            }
            let now = mic.host_ns.max(render.host_ns) + 10_000_000;
            owner.observe_render(render, now, &mut output).unwrap();
            owner.observe_mic(mic, now, &mut output).unwrap();
            let far: Vec<_> = (0..160).map(|n| wave(render.device_frames + n)).collect();
            let captured: Vec<_> = (0..160)
                .map(|n| wave(mic.device_frames + n) + 0.05)
                .collect();
            owner
                .append_render(i * 160, &far, now, &mut output)
                .unwrap();
            owner
                .push_mic(2192 + i * 160, &captured, now, &mut output)
                .unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while owner.worker.pending_samples() > 0 && std::time::Instant::now() < until {
                owner.poll(now, &mut output).unwrap();
                std::thread::yield_now();
            }
        }
        finish(&mut owner, 6_600_000_000, &mut output);
        let mut after_recovery = 0;
        for block in output {
            if block.start_sample >= 2192 + 450 * 160 && block.kind == OutputKind::Cleaned {
                after_recovery += block.samples.len();
                assert!(
                    block.samples.iter().all(|s| (*s - 0.05).abs() < 0.0001),
                    "accepted reference positions diverged after ring loss"
                );
            }
        }
        assert!(
            after_recovery > 8000,
            "healthy processing must resume after fresh timing evidence"
        );
    }
}
