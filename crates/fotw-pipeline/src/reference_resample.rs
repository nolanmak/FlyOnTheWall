//! Bounded worker-side, bandlimited reference lookup on the microphone processing clock.
use crate::reference_clock::{Estimate, Position};
use std::collections::VecDeque;

/// Native mono capture-frame index; never an interleaved sample index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeFrame(pub u64);
/// Delay-compensated mono 16kHz processing-frame index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessingFrame(pub u64);
/// Shared capture-host timestamp in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostNs(pub u64);
/// Tap generation assigned by the capture owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapGeneration(pub u64);
/// Alignment lifetime; advances on timing faults even if taps have not restarted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlignmentEpoch(pub u64);

/// Explicit mapping after microphone conversion. The owner must establish the
/// compensated origin; this object does not infer converter delay or gap padding.
#[derive(Clone, Copy, Debug)]
pub struct FrameDomain {
    /// Native integer frame at the processing origin.
    pub native_origin: NativeFrame,
    /// Signed native-frame offset for converter phase/delay, bounded to ±256.
    /// Obtain it from the converter's geometry; do not assume integer trim is exact.
    pub native_offset_frames: f64,
    /// First delay-compensated processing frame for this mapping.
    pub processing_origin: ProcessingFrame,
    /// Nominal microphone capture rate, before conversion to 16kHz.
    pub native_rate_hz: u32,
}

/// Preallocated history and filter policy. Input is native-rate mono reference
/// PCM; microphone samples themselves never pass through this component.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Nominal render capture rate.
    pub render_rate_hz: u32,
    /// Retained native mono frames, at most two seconds.
    pub capacity_frames: usize,
    /// Microphone conversion origin/rate.
    pub mic: FrameDomain,
    /// Render tap generation.
    pub render_generation: TapGeneration,
    /// Microphone tap generation.
    pub mic_generation: TapGeneration,
    /// Current alignment lifetime.
    pub epoch: AlignmentEpoch,
}
/// All timing arguments are named; clocks must share the same capture-host epoch.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    /// Alignment lifetime that owns this work.
    pub epoch: AlignmentEpoch,
    /// Original microphone processing position, never shifted for filter support.
    pub start: ProcessingFrame,
    /// Current shared capture-host time.
    pub now: HostNs,
    /// Fresh render-native clock estimate.
    pub render: Estimate,
    /// Fresh microphone-native clock estimate.
    pub mic: Estimate,
}
/// Refusal never partially changes caller output or admitted reference history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Unsupported format, filter/history budget or coordinate origin.
    Config,
    /// Work belongs to another alignment lifetime.
    Epoch,
    /// Estimate or write belongs to another tap generation.
    Generation,
    /// Stale, mismatched or unqualified clock geometry.
    Clock,
    /// Noncontiguous reference writes or overflowing positions.
    Positions,
    /// Full interpolation support is not retained/captured; never pad as real audio.
    MissingReference,
    /// Input or output cannot be represented as finite PCM.
    Samples,
    /// Processing exceeds 3200 output frames or one second of native input.
    Block,
}

/// Fixed-budget reference history. Allocate/preload on a control/worker thread;
/// append and lookup do not allocate, lock, log, or touch microphone PCM.
/// Filtering remains worker work despite allocation freedom; never call it on
/// the realtime capture callback.
pub struct ReferenceBuffer {
    config: Config,
    history: VecDeque<f32>,
    start: u64,
    end: u64,
    taps: usize,
    kernels: Vec<Vec<f32>>,
    positions: Vec<Position>,
    scratch: Vec<f32>,
}
const PROCESSING_RATE: u32 = 16000;
const PHASES: usize = 256;
const MAX_OUTPUT: usize = 3200;

fn domain_valid(domain: FrameDomain) -> bool {
    (4000..=768000).contains(&domain.native_rate_hz)
        && domain.native_offset_frames.is_finite()
        && (-256.0..=256.0).contains(&domain.native_offset_frames)
}

impl ReferenceBuffer {
    /// Preallocate filter and reference buffers; supported native rates are4–768kHz.
    pub fn new(config: Config) -> Result<Self, Error> {
        if !(4000..=768000).contains(&config.render_rate_hz) || !domain_valid(config.mic) {
            return Err(Error::Config);
        }
        let taps = 64 * config.render_rate_hz.div_ceil(PROCESSING_RATE) as usize;
        if config.capacity_frames < taps
            || config.capacity_frames > config.render_rate_hz as usize * 2
        {
            return Err(Error::Config);
        }
        let radius = taps / 2;
        let cutoff = 0.9 * (f64::from(PROCESSING_RATE) / f64::from(config.render_rate_hz)).min(1.0);
        // Original normalized Blackman-windowed sinc, evaluated before capture.
        // Increasing support with input rate keeps the physical transition width
        // bounded. Include phase1 so interpolation across integer boundaries agrees.
        let mut kernels = Vec::with_capacity(PHASES + 1);
        for phase in 0..=PHASES {
            let fraction = phase as f64 / PHASES as f64;
            let mut row = Vec::with_capacity(taps);
            let mut total = 0.0;
            for tap in 0..taps {
                let d = tap as f64 - (radius - 1) as f64 - fraction;
                let angle = std::f64::consts::PI * d / radius as f64;
                let window = if d.abs() > radius as f64 {
                    0.0
                } else {
                    0.42 + 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos()
                };
                let x = std::f64::consts::PI * cutoff * d;
                let value = cutoff * if x.abs() < 1e-12 { 1.0 } else { x.sin() / x } * window;
                total += value;
                row.push(value);
            }
            kernels.push(row.into_iter().map(|v| (v / total) as f32).collect());
        }
        Ok(Self {
            config,
            history: VecDeque::with_capacity(config.capacity_frames),
            start: 0,
            end: 0,
            taps,
            kernels,
            positions: vec![
                Position {
                    frame: 0,
                    fraction: 0.0
                };
                MAX_OUTPUT
            ],
            scratch: vec![0.0; MAX_OUTPUT],
        })
    }
    /// Admit consecutive native mono reference frames under an explicit lifetime.
    pub fn append(
        &mut self,
        epoch: AlignmentEpoch,
        generation: TapGeneration,
        start: NativeFrame,
        samples: &[f32],
    ) -> Result<(), Error> {
        if epoch != self.config.epoch {
            return Err(Error::Epoch);
        }
        if generation != self.config.render_generation {
            return Err(Error::Generation);
        }
        if samples.len() > self.config.render_rate_hz as usize {
            return Err(Error::Block);
        }
        let end = start
            .0
            .checked_add(samples.len() as u64)
            .ok_or(Error::Positions)?;
        if !self.history.is_empty() && start.0 != self.end {
            return Err(Error::Positions);
        }
        if samples.iter().any(|v| !v.is_finite()) {
            return Err(Error::Samples);
        }
        if samples.is_empty() {
            return Ok(());
        }
        if self.history.is_empty() {
            self.start = start.0;
        }
        for sample in samples {
            if self.history.len() == self.config.capacity_frames {
                self.history.pop_front();
                self.start += 1;
            }
            self.history.push_back(*sample);
        }
        self.end = end;
        Ok(())
    }
    /// Produce exactly out.len() microphone-clock reference frames, or leave all
    /// output unchanged on refusal. Missing support requires original-mic fallback.
    pub fn fill(&mut self, request: Request, out: &mut [f32]) -> Result<(), Error> {
        if request.epoch != self.config.epoch {
            return Err(Error::Epoch);
        }
        if request.render.generation != self.config.render_generation.0
            || request.mic.generation != self.config.mic_generation.0
        {
            return Err(Error::Generation);
        }
        for (clock, nominal) in [
            (request.render, self.config.render_rate_hz),
            (request.mic, self.config.mic.native_rate_hz),
        ] {
            if !clock.is_fresh(request.now.0)
                || !clock.rate_hz.is_finite()
                || (clock.rate_hz / f64::from(nominal) - 1.0).abs() > 0.000500001
            {
                return Err(Error::Clock);
            }
        }
        if out.len() > MAX_OUTPUT {
            return Err(Error::Block);
        }
        // Validate the complete request before writing even one caller sample.
        for i in 0..out.len() {
            let processing = request
                .start
                .0
                .checked_add(i as u64)
                .ok_or(Error::Positions)?;
            let delta = processing
                .checked_sub(self.config.mic.processing_origin.0)
                .ok_or(Error::Positions)?;
            let scaled = u128::from(delta) * u128::from(self.config.mic.native_rate_hz);
            let native = u64::try_from(scaled / u128::from(PROCESSING_RATE))
                .map_err(|_| Error::Positions)?;
            let fraction = (scaled % u128::from(PROCESSING_RATE)) as f64
                / f64::from(PROCESSING_RATE)
                + self.config.mic.native_offset_frames;
            let native = self
                .config
                .mic
                .native_origin
                .0
                .checked_add(native)
                .ok_or(Error::Positions)?;
            let shift = fraction.floor();
            let native = if shift >= 0.0 {
                native.checked_add(shift as u64)
            } else {
                native.checked_sub((-shift) as u64)
            }
            .ok_or(Error::Positions)?;
            let fraction = fraction - shift;
            let a = request
                .render
                .map_frame_from(&request.mic, native, request.now.0)
                .ok_or(Error::Clock)?;
            let position = if fraction == 0.0 {
                a
            } else {
                let b = request
                    .render
                    .map_frame_from(
                        &request.mic,
                        native.checked_add(1).ok_or(Error::Positions)?,
                        request.now.0,
                    )
                    .ok_or(Error::Clock)?;
                let step = b.frame.checked_sub(a.frame).ok_or(Error::Clock)? as f64 + b.fraction
                    - a.fraction;
                let sub = a.fraction + step * fraction;
                Position {
                    frame: a
                        .frame
                        .checked_add(sub.floor() as u64)
                        .ok_or(Error::Positions)?,
                    fraction: sub.fract(),
                }
            };
            let left = position
                .frame
                .checked_sub((self.taps / 2 - 1) as u64)
                .ok_or(Error::MissingReference)?;
            let right = left
                .checked_add(self.taps as u64 - 1)
                .ok_or(Error::Positions)?;
            // Every filter tap must belong to the qualified clock interval,
            // including when its center has already crossed a recovery fence.
            if request.render.host_at_frame(left).is_none()
                || request.render.host_at_frame(right).is_none()
            {
                return Err(Error::Clock);
            }
            let index = left
                .checked_sub(self.start)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or(Error::MissingReference)?;
            if index
                .checked_add(self.taps)
                .is_none_or(|end| end > self.history.len())
            {
                return Err(Error::MissingReference);
            }
            self.positions[i] = position;
        }
        for i in 0..out.len() {
            let p = self.positions[i];
            let index = (p.frame - (self.taps / 2 - 1) as u64 - self.start) as usize;
            let phase = p.fraction * PHASES as f64;
            let row = phase.floor() as usize;
            let mix = phase.fract();
            let mut sum = 0.0;
            for tap in 0..self.taps {
                let a = f64::from(self.kernels[row][tap]);
                let b = f64::from(self.kernels[row + 1][tap]);
                sum += f64::from(self.history[index + tap]) * (a + (b - a) * mix);
            }
            let value = sum as f32;
            if !value.is_finite() {
                return Err(Error::Samples);
            }
            self.scratch[i] = value;
        }
        out.copy_from_slice(&self.scratch[..out.len()]);
        Ok(())
    }
    /// Retained native reference frames; never exceeds the configured budget.
    pub fn retained_frames(&self) -> usize {
        self.history.len()
    }
    /// Fence old work and discard history. A render rate change needs a new
    /// preloaded buffer; microphone conversion mapping may change on reset.
    pub fn reset(
        &mut self,
        epoch: AlignmentEpoch,
        render: TapGeneration,
        mic: TapGeneration,
        domain: FrameDomain,
    ) -> Result<(), Error> {
        if epoch.0 <= self.config.epoch.0 {
            return Err(Error::Epoch);
        }
        if render.0 < self.config.render_generation.0 || mic.0 < self.config.mic_generation.0 {
            return Err(Error::Generation);
        }
        if !domain_valid(domain) {
            return Err(Error::Config);
        }
        self.config.epoch = epoch;
        self.config.render_generation = render;
        self.config.mic_generation = mic;
        self.config.mic = domain;
        self.history.clear();
        self.start = 0;
        self.end = 0;
        Ok(())
    }
}
