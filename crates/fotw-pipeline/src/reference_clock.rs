//! Worker-side rate fitting from qualified capture observations, never callback arrival order.
use crate::ring::CaptureObservation;
use std::collections::VecDeque;

/// An observation cannot extend a trusted clock through these conditions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// Invalid nominal rate/channel configuration.
    Format,
    /// Observation belongs to an older generation or callback.
    Superseded,
    /// Timestamp is future or older than one second on the shared host clock.
    Stale,
    /// Explicit or superseded timing fault, or ring drop.
    Fault,
    /// Frame/sample positions do not describe a continuous admitted stream.
    Positions,
    /// Measured rate exceeds 500ppm of nominal or residual exceeds 100us.
    Fit,
}

/// Fractional position without converting an absolute u64 sample counter to f64.
#[derive(Clone, Copy, Debug)]
pub struct Position {
    /// Sample immediately before or at the requested time.
    pub frame: u64,
    /// Fraction of the next sample, in [0,1).
    pub fraction: f64,
}

/// Affine clock estimate bounded to observed, accepted mono frame positions.
#[derive(Clone, Copy, Debug)]
pub struct Estimate {
    /// Owner-supplied tap generation.
    pub generation: u64,
    /// Measured frames per second, not the nominal format rate.
    pub rate_hz: f64,
    /// Maximum absolute fit residual over retained observations, in nanoseconds.
    pub residual_ns: f64,
    origin_host: u64,
    origin_frame: u64,
    intercept: f64,
    last_frame: u64,
    latest_host: u64,
}
impl Estimate {
    /// Map an accepted mono frame into (integer host epoch ns, relative seconds).
    /// Keeping the epoch separate avoids losing precision at large host counters.
    pub fn host_at_frame(&self, frame: u64) -> Option<(u64, f64)> {
        if frame < self.origin_frame || frame > self.last_frame {
            return None;
        }
        Some((
            self.origin_host,
            ((frame - self.origin_frame) as f64 - self.intercept) / self.rate_hz,
        ))
    }
    /// Whether the latest evidence is no more than one second old, never future.
    pub fn is_fresh(&self, now_ns: u64) -> bool {
        now_ns
            .checked_sub(self.latest_host)
            .is_some_and(|age| age <= 1_000_000_000)
    }
    /// Map another stream's accepted frame into this stream, requiring fresh
    /// evidence for both clocks. Owners must also fence cached estimates when
    /// either generation, timing-fault or drop counter changes.
    pub fn map_frame_from(&self, other: &Self, frame: u64, now_ns: u64) -> Option<Position> {
        if !self.is_fresh(now_ns) || !other.is_fresh(now_ns) {
            return None;
        }
        let (epoch, seconds) = other.host_at_frame(frame)?;
        let difference = epoch.abs_diff(self.origin_host);
        // Trackers retain at most eight seconds, so distant epochs cannot overlap.
        if difference > 10_000_000_000 {
            return None;
        }
        let sign = if epoch >= self.origin_host { 1.0 } else { -1.0 };
        let seconds = seconds + sign * difference as f64 / 1e9;
        let delta = self.rate_hz.mul_add(seconds, self.intercept);
        let end = (self.last_frame - self.origin_frame) as f64;
        if !delta.is_finite() || delta < -1e-7 || delta > end + 1e-7 {
            return None;
        }
        let delta = delta.clamp(0.0, end);
        let rounded = delta.round();
        let delta = if (delta - rounded).abs() < 1e-7 {
            rounded
        } else {
            delta
        };
        Some(Position {
            frame: self.origin_frame.checked_add(delta.floor() as u64)?,
            fraction: delta.fract(),
        })
    }
}

/// Bounded clock evidence retained on a worker. A new generation resets fitting.
/// Native timing flags and cumulative counters are admission evidence, not an
/// acoustic alignment guarantee. Call observe with the current shared host time.
pub struct Tracker {
    nominal: u32,
    channels: u16,
    generation: Option<u64>,
    last: Option<CaptureObservation>,
    points: VecDeque<CaptureObservation>,
    fit: Option<Estimate>,
}
impl Tracker {
    /// Number of retained observations, always bounded by 64.
    pub fn retained_observations(&self) -> usize {
        self.points.len()
    }
    /// Preallocate 64 observations. Collect at most one per 100ms, require a
    /// two-second span, and retain no recording content.
    pub fn new(nominal: u32, channels: u16) -> Result<Self, Rejection> {
        if !(4000..=768000).contains(&nominal) || channels == 0 {
            return Err(Rejection::Format);
        }
        Ok(Self {
            nominal,
            channels,
            generation: None,
            last: None,
            points: VecDeque::with_capacity(64),
            fit: None,
        })
    }
    /// Admit coherent latest-value evidence. Repeated snapshots do not extend
    /// freshness. Err means this call must not use a previous estimate.
    pub fn observe(
        &mut self,
        generation: u64,
        observation: CaptureObservation,
        now_ns: u64,
    ) -> Result<Option<Estimate>, Rejection> {
        if self.generation.is_some_and(|old| generation < old) {
            return Err(Rejection::Superseded);
        }
        if self.generation != Some(generation) {
            self.clear();
            self.generation = Some(generation);
        }
        let o = observation;
        if now_ns
            .checked_sub(o.host_ns)
            .is_none_or(|age| age > 1_000_000_000)
        {
            self.clear();
            return Err(Rejection::Stale);
        }
        if let Some(last) = self.last {
            if o.sequence < last.sequence {
                return Err(Rejection::Superseded);
            }
            if o.sequence == last.sequence {
                if o == last {
                    return Ok(self.fit);
                }
                self.clear();
                return Err(Rejection::Positions);
            }
            if o.timing_faults != last.timing_faults || o.dropped_samples != last.dropped_samples {
                self.clear();
                return Err(Rejection::Fault);
            }
            let samples = o.sample_start.checked_sub(last.sample_start);
            let frames = o.device_frames.checked_sub(last.device_frames);
            let skipped = o.sequence - last.sequence - 1;
            let minimum = skipped
                .checked_mul(u64::from(self.channels))
                .and_then(|n| n.checked_add(last.sample_count as u64));
            if o.host_ns <= last.host_ns
                || samples.zip(minimum).is_none_or(|(n, min)| n < min)
                || (skipped == 0 && samples != Some(last.sample_count as u64))
                || frames.and_then(|n| n.checked_mul(u64::from(self.channels))) != samples
            {
                self.clear();
                return Err(Rejection::Positions);
            }
        }
        if o.flags & !1 != 0 || o.sample_count == 0 {
            self.clear();
            return Err(Rejection::Fault);
        }
        let channels = u64::from(self.channels);
        if o.sequence == 0
            || !o.sample_start.is_multiple_of(channels)
            || !(o.sample_count as u64).is_multiple_of(channels)
            || o.sample_count as u64 > u64::from(self.nominal) * channels
            || o.sample_start.checked_add(o.sample_count as u64).is_none()
        {
            self.clear();
            return Err(Rejection::Positions);
        }
        self.last = Some(o);
        while self
            .points
            .front()
            .is_some_and(|p| o.host_ns - p.host_ns > 8_000_000_000)
        {
            self.points.pop_front();
        }
        if self
            .points
            .back()
            .is_none_or(|p| o.host_ns - p.host_ns >= 100_000_000)
        {
            if self.points.len() == 64 {
                self.points.pop_front();
            }
            self.points.push_back(o);
        }
        let first = *self
            .points
            .front()
            .expect("first accepted observation is retained");
        if self.points.len() < 8 || o.host_ns - first.host_ns < 2_000_000_000 {
            self.fit = None;
            return Ok(None);
        }
        let origin = first.sample_start / channels;
        let n = self.points.len() as f64;
        let (mut sx, mut sy) = (0.0, 0.0);
        for p in &self.points {
            sx += (p.host_ns - first.host_ns) as f64 / 1e9;
            sy += (p.sample_start / channels - origin) as f64;
        }
        let (mx, my) = (sx / n, sy / n);
        let (mut covariance, mut variance) = (0.0, 0.0);
        for p in &self.points {
            let x = (p.host_ns - first.host_ns) as f64 / 1e9 - mx;
            let y = (p.sample_start / channels - origin) as f64 - my;
            covariance += x * y;
            variance += x * x;
        }
        let rate = covariance / variance;
        let intercept = my - rate * mx;
        let residual = |p: &CaptureObservation| {
            let predicted = rate.mul_add((p.host_ns - first.host_ns) as f64 / 1e9, intercept);
            (predicted - (p.sample_start / channels - origin) as f64).abs() / rate * 1e9
        };
        let error = self
            .points
            .iter()
            .map(residual)
            .fold(residual(&o), f64::max);
        if !rate.is_finite()
            || (rate / f64::from(self.nominal) - 1.0).abs() > 500.0 / 1e6
            || !error.is_finite()
            || error > 100_000.0
        {
            self.clear();
            return Err(Rejection::Fit);
        }
        let fit = Estimate {
            generation,
            rate_hz: rate,
            residual_ns: error,
            origin_host: first.host_ns,
            origin_frame: origin,
            intercept,
            last_frame: o.sample_start / channels + o.sample_count as u64 / channels - 1,
            latest_host: o.host_ns,
        };
        self.fit = Some(fit);
        Ok(Some(fit))
    }
    fn clear(&mut self) {
        self.last = None;
        self.points.clear();
        self.fit = None;
    }
}
