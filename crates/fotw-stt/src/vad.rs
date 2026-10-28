//! The neural speech gate in front of the local model (#242).
//!
//! The [`Endpointer`](crate::local::Endpointer) cuts a leg wherever the level
//! moves like a voice, and music, a cafe or a keyboard move like that too.
//! Whisper fed any of them writes its training-set filler ("Thank you."). So
//! every chunk the endpointer cuts is shown to a speech detector first —
//! Silero VAD in production, a scripted one in tests — and only the stretches
//! it hears as speech reach the model:
//!
//! - a chunk with no speech region is dropped and the model never runs;
//! - inside a chunk that has speech, everything outside the (padded) speech
//!   regions is replaced with digital silence. The chunk keeps its length and
//!   its start sample, so a segment's recording-time offsets are exactly what
//!   they would be without the gate.
//!
//! The idea is Muesli's (`native/MuesliNative/Sources/MuesliNativeApp/
//! TranscriptionRuntime.swift`, `transcribeMeetingChunk`, at commit
//! `97e455794112126a896cae17f5c829e8ffa820c8`, MIT, © 2026 Pranav Hari): skip a
//! chunk when no Silero frame is above 0.5. The region rules below are Silero's
//! own `get_speech_timestamps` hysteresis, which whisper.cpp also uses.

use crate::SttError;
use crate::local::{Recognized, Recognizer, SAMPLE_RATE};

/// Silero's analysis window at 16 kHz: one probability per 32 ms.
pub const WINDOW_SAMPLES: usize = 512;

const SAMPLES_PER_MS: usize = SAMPLE_RATE as usize / 1_000;

/// A model that says, window by window, how likely it is that someone is
/// talking.
pub trait SpeechDetector: Send {
    /// One speech probability in `[0, 1]` for each consecutive
    /// [`WINDOW_SAMPLES`]-sample window of 16 kHz mono `pcm`; a final partial
    /// window counts as a window.
    ///
    /// # Errors
    ///
    /// When the detector fails on this audio.
    fn probabilities(&mut self, pcm: &[f32]) -> Result<Vec<f32>, SttError>;
}

/// Where the gate draws the line between speech and everything else.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateConfig {
    /// A window above this starts a speech region. Silero's and Muesli's 0.5.
    pub threshold: f32,
    /// A region continues while windows stay at or above this, so a soft
    /// syllable inside a word does not split it. Silero uses threshold − 0.15.
    pub neg_threshold: f32,
    /// A region ends after this much audio below `neg_threshold`.
    pub min_silence_ms: u64,
    /// A region shorter than this is a click or a breath, not speech.
    pub min_speech_ms: u64,
    /// Audio kept either side of a region, so word edges the detector scored
    /// late or early still reach the model.
    pub pad_ms: u64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            neg_threshold: 0.35,
            min_silence_ms: 100,
            min_speech_ms: 250,
            pad_ms: 300,
        }
    }
}

/// The sample ranges of `len` samples that the gate keeps, given one
/// probability per [`WINDOW_SAMPLES`] window. Empty when nothing is speech.
#[must_use]
pub fn speech_regions(
    probabilities: &[f32],
    len: usize,
    config: &GateConfig,
) -> Vec<std::ops::Range<usize>> {
    let window_ms = WINDOW_SAMPLES as u64 / SAMPLES_PER_MS as u64;
    let min_silence = config.min_silence_ms.div_ceil(window_ms) as usize;
    let min_speech = (config.min_speech_ms as usize * SAMPLES_PER_MS).max(1);
    let pad = config.pad_ms as usize * SAMPLES_PER_MS;

    // Speech runs in windows, by Silero's rule: start above `threshold`; a
    // window back above it cancels a pending end; the run ends where audio
    // first fell under `neg_threshold`, once that has lasted `min_silence`.
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    let mut quiet_since: Option<usize> = None;
    for (i, &p) in probabilities.iter().enumerate() {
        let Some(begin) = start else {
            if p > config.threshold {
                start = Some(i);
                quiet_since = None;
            }
            continue;
        };
        if p > config.threshold {
            quiet_since = None;
        } else if p < config.neg_threshold && quiet_since.is_none() {
            quiet_since = Some(i);
        }
        if let Some(end) = quiet_since
            && i + 1 - end >= min_silence
        {
            runs.push(begin..end);
            start = None;
            quiet_since = None;
        }
    }
    if let Some(begin) = start {
        runs.push(begin..quiet_since.unwrap_or(probabilities.len()));
    }

    let mut regions: Vec<std::ops::Range<usize>> = Vec::new();
    for run in runs {
        let from = (run.start * WINDOW_SAMPLES).min(len);
        let to = (run.end * WINDOW_SAMPLES).min(len);
        if to - from < min_speech {
            continue;
        }
        let padded = from.saturating_sub(pad)..(to + pad).min(len);
        match regions.last_mut() {
            Some(last) if padded.start <= last.end => last.end = last.end.max(padded.end),
            _ => regions.push(padded),
        }
    }
    regions
}

/// Whether `start_ms..end_ms` of a chunk overlaps any of the gate's speech
/// `regions` (sample ranges in the same chunk).
#[must_use]
pub fn overlaps_speech(start_ms: u64, end_ms: u64, regions: &[std::ops::Range<usize>]) -> bool {
    let from = start_ms as usize * SAMPLES_PER_MS;
    // A zero-length segment still sits somewhere: count its first sample.
    let to = (end_ms as usize * SAMPLES_PER_MS).max(from + 1);
    regions.iter().any(|r| from < r.end && to > r.start)
}

/// A [`Recognizer`] that only lets speech through to the one it wraps.
pub struct GatedRecognizer {
    detector: Box<dyn SpeechDetector>,
    inner: Box<dyn Recognizer>,
    config: GateConfig,
}

impl GatedRecognizer {
    /// Gate `inner` behind `detector`.
    #[must_use]
    pub fn new(
        detector: Box<dyn SpeechDetector>,
        inner: Box<dyn Recognizer>,
        config: GateConfig,
    ) -> Self {
        Self {
            detector,
            inner,
            config,
        }
    }
}

impl Recognizer for GatedRecognizer {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Recognized>, SttError> {
        let probabilities = self.detector.probabilities(pcm)?;
        let regions = speech_regions(&probabilities, pcm.len(), &self.config);
        match regions.as_slice() {
            [] => Ok(Vec::new()),
            [all] if *all == (0..pcm.len()) => self.inner.transcribe_speech(pcm, &regions),
            _ => {
                // Silence in place rather than cut out: the chunk keeps its
                // length, so the model's times inside it stay recording times.
                let mut masked = vec![0.0; pcm.len()];
                for region in &regions {
                    masked[region.clone()].copy_from_slice(&pcm[region.clone()]);
                }
                self.inner.transcribe_speech(&masked, &regions)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows(probabilities: &[(f32, usize)]) -> Vec<f32> {
        probabilities
            .iter()
            .flat_map(|&(p, n)| std::iter::repeat_n(p, n))
            .collect()
    }

    const MS_PER_WINDOW: usize = WINDOW_SAMPLES / SAMPLES_PER_MS;

    #[test]
    fn segment_overlap_with_speech_is_measured_in_chunk_time() {
        let ms = SAMPLES_PER_MS;
        let regions = [1_000 * ms..2_000 * ms, 5_000 * ms..6_000 * ms];
        assert!(overlaps_speech(1_500, 1_600, &regions));
        assert!(overlaps_speech(500, 1_001, &regions));
        assert!(overlaps_speech(1_999, 5_500, &regions));
        assert!(!overlaps_speech(2_000, 5_000, &regions));
        assert!(!overlaps_speech(0, 1_000, &regions));
        assert!(!overlaps_speech(6_000, 9_000, &regions));
        // A zero-length segment inside speech still counts.
        assert!(overlaps_speech(1_500, 1_500, &regions));
    }

    #[test]
    fn nothing_above_the_threshold_is_no_speech() {
        let config = GateConfig::default();
        for p in [0.0, 0.2, 0.49, 0.5] {
            let probs = windows(&[(p, 100)]);
            assert!(
                speech_regions(&probs, 100 * WINDOW_SAMPLES, &config).is_empty(),
                "p = {p}"
            );
        }
    }

    #[test]
    fn a_blip_shorter_than_min_speech_is_not_a_region() {
        let config = GateConfig::default();
        // 4 windows = 128 ms above threshold, inside 3 s of nothing.
        let probs = windows(&[(0.1, 40), (0.95, 4), (0.1, 50)]);
        assert!(speech_regions(&probs, probs.len() * WINDOW_SAMPLES, &config).is_empty());
    }

    #[test]
    fn a_region_is_padded_and_clamped_to_the_audio() {
        let config = GateConfig::default();
        // Speech from window 40 to 70 (1280 ms .. 2240 ms) of 100 windows.
        let probs = windows(&[(0.1, 40), (0.9, 30), (0.1, 30)]);
        let len = probs.len() * WINDOW_SAMPLES;
        let regions = speech_regions(&probs, len, &config);
        let pad = config.pad_ms as usize * SAMPLES_PER_MS;
        assert_eq!(
            regions,
            vec![40 * WINDOW_SAMPLES - pad..70 * WINDOW_SAMPLES + pad]
        );

        // Speech to the very edges: padding never leaves the chunk.
        let probs = windows(&[(0.9, 100)]);
        assert_eq!(speech_regions(&probs, len - 7, &config), vec![0..len - 7]);
    }

    #[test]
    fn a_soft_dip_inside_speech_does_not_split_it_but_a_pause_does() {
        let config = GateConfig::default();
        // A dip to 0.4 (above neg_threshold) for 10 windows stays one region.
        let probs = windows(&[(0.9, 20), (0.4, 10), (0.9, 20)]);
        assert_eq!(
            speech_regions(&probs, probs.len() * WINDOW_SAMPLES, &config).len(),
            1
        );
        // A real pause, well past min_silence plus both pads, makes two.
        let gap = (2 * config.pad_ms as usize + 400) / MS_PER_WINDOW;
        let probs = windows(&[(0.9, 20), (0.05, gap), (0.9, 20)]);
        assert_eq!(
            speech_regions(&probs, probs.len() * WINDOW_SAMPLES, &config).len(),
            2
        );
    }
}
