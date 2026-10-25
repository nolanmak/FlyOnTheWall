//! Worker-side frame/reference adaptation for mono 16 kHz echo cancellation.
//! Original audio must be archived before this stage; never call it on a capture callback.

use std::collections::VecDeque;

/// Fixed backend shape and explicit retained-buffer limits, in mono samples.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Exact number passed to every backend call.
    pub frame_samples: usize,
    /// Backend's declared algorithmic delay, compensated before publication.
    pub delay_samples: usize,
    /// Maximum unprocessed microphone samples waiting for a reference.
    pub max_wait_samples: usize,
    /// Maximum retained far-end reference samples.
    pub max_reference_samples: usize,
}
/// Loaded, worker-owned backend. Render analysis must precede paired capture.
pub trait Backend {
    /// Backend-specific failure; audio adaptation publishes only a degradation reason.
    type Error;
    /// Actual frame size reported by the pinned backend/runtime.
    fn frame_samples(&self) -> usize;
    /// Declared processing latency, validated independently for that backend.
    fn delay_samples(&self) -> usize;
    /// Analyze a fixed frame without mutating the original render audio.
    fn analyze_render(&mut self, frame: &[f32]) -> Result<(), Self::Error>;
    /// Write every sample of a fixed capture frame into separate output storage.
    /// Partial or non-finite output is rejected before publication.
    fn process_capture(&mut self, frame: &[f32], output: &mut [f32]) -> Result<(), Self::Error>;
    /// Clear signal history for an explicit route/generation reset.
    fn reset(&mut self) -> Result<(), Self::Error>;
}
/// Why original microphone samples were passed through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BypassReason {
    /// A usable, aligned reference did not arrive within the retained budget.
    MissingReference,
    /// A backend call failed or returned invalid output.
    BackendFailed,
    /// Route reset releases pending original audio before clearing history.
    Reset,
}
/// Provenance of one output interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    /// Successfully processed and delay-compensated samples.
    Cleaned,
    /// Original samples retained after a degraded processing episode.
    Raw(BypassReason),
}
/// Backend invocation health; reference availability and acoustic quality are
/// separate conditions and are not established by this state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendState {
    /// The preloaded backend has not failed and may accept paired frames.
    Available,
    /// Calls are fenced; original audio passes through until explicit reset.
    Failed,
}
/// One interval on the original microphone sample clock.
#[derive(Debug)]
pub struct Output {
    /// Recording/route generation supplied by the owner.
    pub epoch: u64,
    /// Original absolute sample offset; never shifted to backend output time.
    pub start_sample: u64,
    /// Owned output; caller drains it separately from bounded adapter storage.
    pub samples: Vec<f32>,
    /// Whether processing succeeded or original audio was retained.
    pub kind: OutputKind,
}
/// Lifetime processing counters, independent from caller-owned output storage.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Original samples successfully processed and published.
    pub cleaned_samples: u64,
    /// Original samples published without processing.
    pub raw_samples: u64,
    /// Failed backend episodes, counted once until explicit reset.
    pub backend_failures: u64,
}
/// Invalid input is rejected before accepting any samples from that call.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// Backend shape or retained-buffer limits exceed supported bounds.
    #[error("invalid AEC frame or buffer configuration")]
    Config,
    /// Stop was already finalized.
    #[error("AEC adapter is closed")]
    Closed,
    /// Input belongs to another route generation.
    #[error("stale AEC generation")]
    Epoch,
    /// Microphone input must remain contiguous on its original clock.
    #[error("discontinuous AEC input")]
    Discontinuous,
    /// Absolute sample arithmetic would overflow.
    #[error("AEC clock overflow")]
    Overflow,
    /// Invalid floating-point input is not accepted by the frame backend.
    #[error("non-finite AEC input")]
    InvalidSamples,
}
/// Bounded worker-side adapter. Backends are preloaded outside this object.
pub struct Adapter<B: Backend> {
    backend: B,
    config: Config,
    epoch: u64,
    end: u64,
    emitted: u64,
    submitted: u64,
    reference_start: u64,
    reference_end: u64,
    raw: VecDeque<f32>,
    reference: VecDeque<f32>,
    render_frame: Vec<f32>,
    capture_frame: Vec<f32>,
    processed_frame: Vec<f32>,
    skip: usize,
    backend_has_history: bool,
    failed: bool,
    closed: bool,
    stats: Stats,
}
impl<B: Backend> Adapter<B> {
    /// Start at an original absolute sample offset with an already loaded backend.
    pub fn new(backend: B, config: Config, epoch: u64, start: u64) -> Result<Self, Error> {
        if config.frame_samples == 0
            || config.frame_samples > 1024
            || config.delay_samples > 16000
            || config.max_wait_samples < config.frame_samples
            || config.max_wait_samples > 3200
            || config.max_reference_samples < config.frame_samples
            || config.max_reference_samples > 16000
            || config.frame_samples != backend.frame_samples()
            || config.delay_samples != backend.delay_samples()
        {
            return Err(Error::Config);
        }
        Ok(Self {
            backend,
            config,
            epoch,
            end: start,
            emitted: start,
            submitted: start,
            reference_start: start,
            reference_end: start,
            raw: VecDeque::with_capacity(
                config.delay_samples + config.max_wait_samples + config.frame_samples,
            ),
            reference: VecDeque::with_capacity(config.max_reference_samples),
            render_frame: vec![0.0; config.frame_samples],
            capture_frame: vec![0.0; config.frame_samples],
            processed_frame: vec![0.0; config.frame_samples],
            skip: config.delay_samples,
            backend_has_history: false,
            failed: false,
            closed: false,
            stats: Stats::default(),
        })
    }
    /// Supply independently delivered reference audio on the same source clock.
    pub fn push_render(
        &mut self,
        epoch: u64,
        start: u64,
        samples: &[f32],
        out: &mut Vec<Output>,
    ) -> Result<(), Error> {
        self.validate(epoch, start, samples)?;
        if start < self.reference_end {
            return Err(Error::Discontinuous);
        }
        if samples.is_empty() {
            return Ok(());
        }
        if start > self.reference_end {
            self.reference.clear();
            self.reference_start = start;
            self.reference_end = start;
        }
        for sample in samples {
            if self.failed {
                self.reference.clear();
                self.reference_end = start + samples.len() as u64;
                self.reference_start = self.reference_end;
                break;
            }
            // Evict before pushing so a full queue never doubles its backing
            // allocation just to discard the oldest reference immediately.
            while self.reference.len() >= self.config.max_reference_samples {
                self.reference.pop_front();
                self.reference_start += 1;
            }
            self.reference.push_back(*sample);
            self.reference_end += 1;
            self.pump(false, out);
        }
        Ok(())
    }
    /// Accept microphone audio and emit any now-ready intervals.
    pub fn push_capture(
        &mut self,
        epoch: u64,
        start: u64,
        samples: &[f32],
        out: &mut Vec<Output>,
    ) -> Result<(), Error> {
        self.validate(epoch, start, samples)?;
        if start != self.end {
            return Err(Error::Discontinuous);
        }
        // Incremental acceptance keeps retained input bounded even when a
        // caller supplies a callback much larger than either queue budget.
        for (at, sample) in samples.iter().enumerate() {
            if self.failed {
                // A failed backend is fenced until explicit reset. Copy the
                // rest directly to caller-owned output, without queue growth
                // or allocating one bypass interval per microphone sample.
                let rest = &samples[at..];
                self.end += rest.len() as u64;
                self.submitted = self.end;
                self.append(
                    rest.to_vec(),
                    OutputKind::Raw(BypassReason::BackendFailed),
                    out,
                );
                self.stats.raw_samples = self.stats.raw_samples.saturating_add(rest.len() as u64);
                break;
            }
            self.raw.push_back(*sample);
            self.end += 1;
            self.pump(false, out);
        }
        Ok(())
    }
    /// Stop once, draining original duration without publishing padded samples.
    pub fn finish(&mut self, out: &mut Vec<Output>) {
        if self.closed {
            return;
        }
        self.pump(true, out);
        // Flush the declared algorithmic delay. Only pending original samples
        // can be published; padding never advances the original sample clock.
        for _ in 0..self
            .config
            .delay_samples
            .div_ceil(self.config.frame_samples)
            + 1
        {
            if self.raw.is_empty() || self.failed {
                break;
            }
            self.render_frame.fill(0.0);
            self.capture_frame.fill(0.0);
            if !self.process(out) {
                break;
            }
            self.emit_processed(out);
        }
        if !self.raw.is_empty() {
            self.bypass(BypassReason::BackendFailed, false, out);
        }
        self.reference.clear();
        self.reference_start = self.reference_end;
        self.closed = true;
    }
    /// Owner-driven wall-clock deadline for a stopped/stalled reference stream.
    /// Sample budgets also enforce this automatically while capture continues.
    pub fn flush_reference_wait(&mut self, out: &mut Vec<Output>) {
        if self.end > self.submitted {
            self.bypass(BypassReason::MissingReference, true, out);
        }
    }
    /// Release pending originals before changing route generation and sample clock.
    /// The accepted generation changes even when backend reset fails. The
    /// returned state then explicitly reports raw passthrough, never readiness.
    pub fn reset(
        &mut self,
        epoch: u64,
        start: u64,
        out: &mut Vec<Output>,
    ) -> Result<BackendState, Error> {
        if epoch <= self.epoch {
            return Err(Error::Epoch);
        }
        self.bypass(BypassReason::Reset, false, out);
        self.epoch = epoch;
        self.end = start;
        self.emitted = start;
        self.submitted = start;
        self.reference.clear();
        self.reference_start = start;
        self.reference_end = start;
        self.skip = self.config.delay_samples;
        self.closed = false;
        self.reset_backend();
        Ok(self.backend_state())
    }
    /// Whether backend calls are currently available or fenced after failure.
    pub fn backend_state(&self) -> BackendState {
        if self.failed {
            BackendState::Failed
        } else {
            BackendState::Available
        }
    }
    /// Retained input samples, excluding fixed work frames, backend-owned
    /// state and the caller's output. Capacity is independent of meeting length.
    pub fn buffered_samples(&self) -> usize {
        self.raw.len() + self.reference.len()
    }
    /// Current processing counters.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    fn validate(&self, epoch: u64, start: u64, samples: &[f32]) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if epoch != self.epoch {
            return Err(Error::Epoch);
        }
        start
            .checked_add(u64::try_from(samples.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if samples.iter().any(|n| !n.is_finite()) {
            return Err(Error::InvalidSamples);
        }
        Ok(())
    }
    fn pump(&mut self, stopping: bool, out: &mut Vec<Output>) {
        if self.failed {
            self.bypass(BypassReason::BackendFailed, false, out);
            return;
        }
        loop {
            let unprocessed = (self.end - self.submitted) as usize;
            if unprocessed == 0 || (unprocessed < self.config.frame_samples && !stopping) {
                break;
            }
            let n = unprocessed.min(self.config.frame_samples);
            if self.submitted < self.reference_start
                || self.submitted + n as u64 > self.reference_end
            {
                if stopping
                    || self.submitted < self.reference_start
                    || unprocessed >= self.config.max_wait_samples
                {
                    self.bypass(BypassReason::MissingReference, true, out);
                }
                break;
            }
            self.capture_frame.fill(0.0);
            self.render_frame.fill(0.0);
            let mic_offset = (self.submitted - self.emitted) as usize;
            let ref_offset = (self.submitted - self.reference_start) as usize;
            for (dst, src) in self.capture_frame[..n]
                .iter_mut()
                .zip(self.raw.iter().skip(mic_offset))
            {
                *dst = *src;
            }
            for (dst, src) in self.render_frame[..n]
                .iter_mut()
                .zip(self.reference.iter().skip(ref_offset))
            {
                *dst = *src;
            }
            if !self.process(out) {
                break;
            }
            self.submitted += n as u64;
            self.emit_processed(out);
            while self.reference_start < self.submitted && !self.reference.is_empty() {
                self.reference.pop_front();
                self.reference_start += 1;
            }
        }
    }
    fn process(&mut self, out: &mut Vec<Output>) -> bool {
        self.backend_has_history = true;
        // A successful return must not disguise an unwritten output suffix as
        // valid silence or stale samples left over from the preceding frame.
        self.processed_frame.fill(f32::NAN);
        // Revalidate after native state transitions too: a route reset or an
        // analysis call must not silently change the configured frame clock.
        let success = self.shape_matches()
            && self.backend.analyze_render(&self.render_frame).is_ok()
            && self.shape_matches()
            && self
                .backend
                .process_capture(&self.capture_frame, &mut self.processed_frame)
                .is_ok()
            && self.shape_matches();
        if !success || self.processed_frame.iter().any(|n| !n.is_finite()) {
            self.failed = true;
            self.stats.backend_failures = self.stats.backend_failures.saturating_add(1);
            self.bypass(BypassReason::BackendFailed, false, out);
            false
        } else {
            true
        }
    }
    fn emit_processed(&mut self, out: &mut Vec<Output>) {
        let skip = self.skip.min(self.processed_frame.len());
        self.skip -= skip;
        let count = (self.processed_frame.len() - skip).min(self.raw.len());
        if count == 0 {
            return;
        }
        let samples = self.processed_frame[skip..skip + count].to_vec();
        self.raw.drain(..count);
        self.append(samples, OutputKind::Cleaned, out);
        self.stats.cleaned_samples = self.stats.cleaned_samples.saturating_add(count as u64);
    }
    fn bypass(&mut self, reason: BypassReason, reset: bool, out: &mut Vec<Output>) {
        let samples: Vec<_> = self.raw.drain(..).collect();
        let n = samples.len();
        self.append(samples, OutputKind::Raw(reason), out);
        self.stats.raw_samples = self.stats.raw_samples.saturating_add(n as u64);
        self.submitted = self.end;
        self.skip = self.config.delay_samples;
        if reset && self.backend_has_history && !self.failed {
            self.reset_backend();
        }
    }
    fn shape_matches(&self) -> bool {
        self.backend.frame_samples() == self.config.frame_samples
            && self.backend.delay_samples() == self.config.delay_samples
    }
    fn reset_backend(&mut self) {
        self.backend_has_history = false;
        self.failed = self.backend.reset().is_err() || !self.shape_matches();
        if self.failed {
            self.stats.backend_failures = self.stats.backend_failures.saturating_add(1);
        }
    }
    fn append(&mut self, samples: Vec<f32>, kind: OutputKind, out: &mut Vec<Output>) {
        if samples.is_empty() {
            return;
        }
        let start = self.emitted;
        self.emitted += samples.len() as u64;
        if let Some(last) = out.last_mut()
            && last.epoch == self.epoch
            && last.kind == kind
            && last.start_sample.checked_add(last.samples.len() as u64) == Some(start)
        {
            last.samples.extend(samples);
        } else {
            out.push(Output {
                epoch: self.epoch,
                start_sample: start,
                samples,
                kind,
            });
        }
    }
}
