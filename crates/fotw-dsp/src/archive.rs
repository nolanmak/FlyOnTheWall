//! Conversion on the pump thread into a track's fixed archive format.
//!
//! One instance belongs to one tap generation. Finish it before a gap or format
//! boundary; the next generation gets a fresh filter. Output delay is trimmed
//! and the final tail is flushed to floor(input_frames * output_rate/input_rate),
//! so filter latency and partial FFT blocks cannot change recording duration.
use crate::resample::ResampleError;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

/// Native-frame coordinate of output frame zero after integer delay trimming.
/// This reports the pinned FFT filter geometry; it does not alter PCM or recipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConversionOrigin {
    /// Signed numerator in native input frames; negative means padded pre-origin support.
    pub input_frame_numerator: i128,
    /// Positive denominator of the native-frame offset.
    pub denominator: u64,
}

pub struct ArchiveConverter {
    input_rate: u32,
    input_channels: u16,
    output_rate: u32,
    output_channels: u16,
    filter: Option<Fft<f32>>,
    pending: Vec<f32>,
    trim_frames: usize,
    input_samples: u64,
    output_frames: u64,
    finished: bool,
}
impl ArchiveConverter {
    /// Residual input-frame origin after the converter's integer output trim.
    /// Rubato5 centers its kernel at floor(fft_size_in/2), while output_delay
    /// trims floor(fft_size_out/2). These differ fractionally for some rate pairs.
    /// The value is stable before, during and after processing/finish.
    pub fn output_origin(&self) -> ConversionOrigin {
        match &self.filter {
            None => ConversionOrigin {
                input_frame_numerator: 0,
                denominator: 1,
            },
            Some(filter) => {
                let input = filter.fft_size_in() as i128;
                let output = filter.fft_size_out() as i128;
                let numerator = filter.output_delay() as i128 * input - (input / 2) * output;
                let (mut a, mut b) = (numerator.unsigned_abs(), output as u128);
                while b != 0 {
                    let remainder = a % b;
                    a = b;
                    b = remainder;
                }
                ConversionOrigin {
                    input_frame_numerator: numerator / a as i128,
                    denominator: (output / a as i128) as u64,
                }
            }
        }
    }
    pub fn new(
        input_rate: u32,
        input_channels: u16,
        output_rate: u32,
        output_channels: u16,
    ) -> Result<Self, ResampleError> {
        for rate in [input_rate, output_rate] {
            if !(4_000..=768_000).contains(&rate) {
                return Err(ResampleError::BadRate(rate));
            }
        }
        for channels in [input_channels, output_channels] {
            if !(1..=32).contains(&channels) {
                return Err(ResampleError::BadChannels(channels));
            }
        }
        let filter = if input_rate == output_rate {
            None
        } else {
            Some(
                Fft::new(
                    input_rate as usize,
                    output_rate as usize,
                    (output_rate / 100) as usize,
                    input_channels as usize,
                    FixedSync::Output,
                )
                .map_err(|e| ResampleError::Backend(e.to_string()))?,
            )
        };
        let trim_frames = filter.as_ref().map_or(0, Resampler::output_delay);
        Ok(Self {
            input_rate,
            input_channels,
            output_rate,
            output_channels,
            filter,
            pending: Vec::new(),
            trim_frames,
            input_samples: 0,
            output_frames: 0,
            finished: false,
        })
    }
    pub fn process(&mut self, pcm: &[f32]) -> Result<Vec<f32>, ResampleError> {
        if self.finished {
            return Err(ResampleError::Backend(
                "archive converter already finished".into(),
            ));
        }
        if pcm.iter().any(|v| !v.is_finite()) {
            return Err(ResampleError::Backend("non-finite capture samples".into()));
        }
        self.input_samples = self
            .input_samples
            .checked_add(pcm.len() as u64)
            .ok_or_else(|| ResampleError::Backend("archive sample count overflow".into()))?;
        self.pending.extend_from_slice(pcm);
        let mut output = Vec::new();
        self.drain(false, &mut output)?;
        Ok(output)
    }
    pub fn finish(&mut self) -> Result<Vec<f32>, ResampleError> {
        if self.finished {
            return Ok(Vec::new());
        }
        if !self
            .input_samples
            .is_multiple_of(u64::from(self.input_channels))
        {
            return Err(ResampleError::Backend(
                "incomplete final capture frame".into(),
            ));
        }
        let mut output = Vec::new();
        self.drain(true, &mut output)?;
        self.finished = true;
        self.pending.clear();
        Ok(output)
    }
    fn target_frames(&self) -> u64 {
        let frames = self.input_samples / u64::from(self.input_channels);
        ((u128::from(frames) * u128::from(self.output_rate)) / u128::from(self.input_rate)) as u64
    }
    fn drain(&mut self, finishing: bool, output: &mut Vec<f32>) -> Result<(), ResampleError> {
        let channels = usize::from(self.input_channels);
        if self.filter.is_none() {
            let length = self.pending.len() / channels * channels;
            let frames: Vec<_> = self.pending.drain(..length).collect();
            self.append(&frames, output);
            return Ok(());
        }
        loop {
            if finishing && self.output_frames >= self.target_frames() {
                break;
            }
            let filter = self.filter.as_mut().expect("filter present");
            let needed = filter.input_frames_next();
            if self.pending.len() < needed * channels {
                if !finishing {
                    break;
                }
                self.pending.resize(needed * channels, 0.0);
            }
            let block: Vec<_> = self.pending.drain(..needed * channels).collect();
            let adapter = InterleavedSlice::new(&block, channels, needed)
                .map_err(|e| ResampleError::Backend(e.to_string()))?;
            let result = filter
                .process(&adapter, None)
                .map_err(|e| ResampleError::Backend(e.to_string()))?
                .take_data();
            let trim = self.trim_frames.min(result.len() / channels);
            self.trim_frames -= trim;
            let available = &result[trim * channels..];
            let take = if finishing {
                (self.target_frames() - self.output_frames).min((available.len() / channels) as u64)
                    as usize
                    * channels
            } else {
                available.len()
            };
            self.append(&available[..take], output);
        }
        Ok(())
    }
    fn append(&mut self, frames: &[f32], output: &mut Vec<f32>) {
        self.output_frames += (frames.len() / usize::from(self.input_channels)) as u64;
        if self.input_channels == self.output_channels {
            output.extend_from_slice(frames);
        } else {
            // Preserve the mean signal when folding channels, and duplicate
            // mono for a wider archive. No channel-position metadata is assumed.
            for frame in frames.chunks_exact(usize::from(self.input_channels)) {
                let mean = frame.iter().map(|x| f64::from(*x)).sum::<f64>()
                    / f64::from(self.input_channels);
                output.extend(std::iter::repeat_n(
                    mean as f32,
                    usize::from(self.output_channels),
                ));
            }
        }
    }
}

/// Capture boundary policy around the strict converter. Invalid driver samples
/// become silence in place; an incomplete final interleaved frame is discarded.
/// A backend failure ends conversion for this generation, while the caller can
/// still finalize the audio already written. Warnings are bounded per generation.
pub struct CaptureArchive {
    converter: ArchiveConverter,
    replaced_samples: u64,
    incomplete_samples: u64,
    failure: Option<String>,
}
impl CaptureArchive {
    /// Active native input shape for this converter/tap generation.
    pub fn input_format(&self) -> (u32, u16) {
        (self.converter.input_rate, self.converter.input_channels)
    }
    /// Exact native-frame origin after archive output delay trimming.
    pub fn output_origin(&self) -> ConversionOrigin {
        self.converter.output_origin()
    }
    pub fn new(
        input_rate: u32,
        input_channels: u16,
        output_rate: u32,
        output_channels: u16,
    ) -> Result<Self, ResampleError> {
        Ok(Self {
            converter: ArchiveConverter::new(
                input_rate,
                input_channels,
                output_rate,
                output_channels,
            )?,
            replaced_samples: 0,
            incomplete_samples: 0,
            failure: None,
        })
    }
    pub fn process(&mut self, pcm: &mut [f32]) -> Vec<f32> {
        for sample in pcm.iter_mut().filter(|sample| !sample.is_finite()) {
            *sample = 0.0;
            self.replaced_samples = self.replaced_samples.saturating_add(1);
        }
        if self.failure.is_some() {
            return Vec::new();
        }
        match self.converter.process(pcm) {
            Ok(pcm) => pcm,
            Err(error) => {
                self.failure = Some(error.to_string());
                Vec::new()
            }
        }
    }
    pub fn finish(&mut self) -> Vec<f32> {
        if self.failure.is_some() {
            return Vec::new();
        }
        // The strict converter keeps partial frames in pending until complete.
        // Remove only that partial frame, retaining all complete buffered frames
        // (including the FFT tail), and keep the strict public finish unchanged.
        let partial = self.converter.input_samples % u64::from(self.converter.input_channels);
        if partial > 0 {
            self.incomplete_samples += partial;
            self.converter.input_samples -= partial;
            let keep = self.converter.pending.len() - partial as usize;
            self.converter.pending.truncate(keep);
        }
        match self.converter.finish() {
            Ok(pcm) => pcm,
            Err(error) => {
                self.failure = Some(error.to_string());
                Vec::new()
            }
        }
    }
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.replaced_samples > 0 {
            warnings.push(format!(
                "{} non-finite capture samples replaced with silence",
                self.replaced_samples
            ));
        }
        if self.incomplete_samples > 0 {
            warnings.push(format!(
                "{} samples discarded from an incomplete final capture frame",
                self.incomplete_samples
            ));
        }
        if let Some(error) = &self.failure {
            warnings.push(format!("archive conversion stopped for this capture generation: {error}; some audio is missing"));
        }
        warnings
    }
}
