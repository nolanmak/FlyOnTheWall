//! Cross-track bleed suppression (#246).
//!
//! **The problem.** A meeting's two tracks are not independent. When the
//! other side's audio carries a quiet copy of the person at this Mac (a
//! remote speakerphone or room system picking them up, a headset that hears
//! its neighbour, a call app that plays the local voice back), the system
//! track holds their words a second time, a little later and much quieter.
//! The live path rarely transcribes that copy; the cleanup, which hears 30 s
//! at a time with more context, does, and the words land on the remote side
//! as insertions. The diarizer hears the same copy as one more voice. On the
//! AMI replays, where each headset hears its neighbours, this was most of
//! what kept the cleanup above the 15% WER and 25% DER gates.
//!
//! **What this does.** Before speech detection, transcription and
//! diarization, a track is read in step with the *other* track (the
//! reference). Wherever the track is, for the moment, only a quieter copy of
//! the reference, those samples are set to zero in place: times are
//! unchanged and nothing else is touched. A frame counts as a copy when (1)
//! the reference is speaking and the track is well below it, (2) over the
//! surrounding second the two tracks' loudness envelopes rise and fall
//! together at a small lag, and (3) the frame is no louder than the copy
//! would be (the reference less the copy's attenuation, plus a margin).
//! Speech by someone else has its own envelope, failing (2), and where it
//! talks over the copy it makes the track louder than the copy alone,
//! failing (3); either way it is kept.
//!
//! **Why envelopes.** As `fotw_pipeline::echo` explains, a copy that went
//! through air, another microphone or a codec is no longer the same waveform,
//! but its 10 ms log-energy envelope survives: attenuation becomes a constant
//! offset, which the correlation's mean removal ignores.
//!
//! **What it does not do.** It removes copies, it does not subtract them:
//! where the remote person and the copy overlap, the remote person's own
//! envelope dominates and the stretch is kept, copy included. Only small lags
//! ([`BleedParams::max_lag_ms`]) are searched, so an echo that comes back
//! over a network a few hundred ms late is not caught.
//!
//! The thresholds were chosen on AMI meetings that are not the acceptance
//! meetings (docs/POST_MEETING_CLEANUP.md).
//!
//! The decision for a sample depends only on the audio within a fixed
//! distance of it, and each block is read with that much context, so the
//! output is the same however the track is read.

use super::windows::PcmSource;

/// Samples per analysis frame: 10 ms at 16 kHz.
const FRAME: usize = 160;

/// The detector's settings. [`BleedParams::default`] is what the cleanup
/// runs; the plan records it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BleedParams {
    /// Half the envelope-correlation window, ms.
    pub context_ms: u32,
    /// Largest lag searched either way between the tracks, ms.
    pub max_lag_ms: u32,
    /// Envelope correlation at or above which the track follows the reference.
    pub min_correlation: f32,
    /// How far below the reference the track must be, dB (both levels
    /// averaged over ±50 ms).
    pub min_gap_db: f32,
    /// Reference level, dB of mean-square s16, below which it is not
    /// speaking and nothing is removed.
    pub min_reference_db: f32,
    /// How much louder than the copy the track may be at a frame and still
    /// be only the copy, dB (the copy's attenuation is measured over the
    /// correlation window).
    pub max_excess_db: f32,
    /// Removal is extended this far either side of a detected copy, ms.
    pub hang_ms: u32,
    /// Remove copies of the mic from the system track.
    pub system: bool,
    /// Remove copies of the system audio from the mic track.
    pub mic: bool,
}

impl Default for BleedParams {
    fn default() -> Self {
        Self {
            context_ms: 1_000,
            max_lag_ms: 10,
            min_correlation: 0.6,
            min_gap_db: 14.0,
            min_reference_db: 30.0,
            max_excess_db: 6.0,
            hang_ms: 200,
            system: true,
            mic: true,
        }
    }
}

impl BleedParams {
    /// Whether `channel`'s track is filtered.
    #[must_use]
    pub fn applies_to(&self, channel: super::turns::Channel) -> bool {
        match channel {
            super::turns::Channel::Mic => self.mic,
            super::turns::Channel::System => self.system,
        }
    }
}

/// Level smoothing either side of a frame, frames.
const LEVEL_HALF: usize = 5;
/// Output produced per block, frames (10 s).
const BLOCK_FRAMES: usize = 1_000;

impl BleedParams {
    fn frames(ms: u32) -> usize {
        ms as usize / 10
    }

    /// Context, in frames, that one frame's decision reads either side.
    fn margin(&self) -> usize {
        Self::frames(self.context_ms)
            + Self::frames(self.max_lag_ms)
            + LEVEL_HALF
            + Self::frames(self.hang_ms)
    }

    /// Which frames of `track` are only a copy of `reference`. Both start at
    /// the same instant; frames past the reference's end have no copy.
    #[must_use]
    pub fn copied_frames(&self, track: &[i16], reference: &[i16]) -> Vec<bool> {
        let n = track.len().div_ceil(FRAME);
        let level = |pcm: &[i16]| -> Vec<f32> {
            (0..n)
                .map(|i| {
                    let from = (i * FRAME).min(pcm.len());
                    let to = ((i + 1) * FRAME).min(pcm.len());
                    let frame = &pcm[from..to];
                    let power = frame
                        .iter()
                        .map(|&x| f64::from(x) * f64::from(x))
                        .sum::<f64>()
                        / FRAME as f64;
                    (10.0 * (power + 1.0).log10()) as f32
                })
                .collect()
        };
        let (t, r) = (level(track), level(reference));
        let smooth = |x: &[f32], i: usize| {
            let from = i.saturating_sub(LEVEL_HALF);
            let to = (i + LEVEL_HALF + 1).min(n);
            x[from..to].iter().sum::<f32>() / (to - from) as f32
        };
        let half = Self::frames(self.context_ms);
        let lags = Self::frames(self.max_lag_ms) as isize;
        let (ts, rs): (Vec<f32>, Vec<f32>) = (0..n).map(|i| (smooth(&t, i), smooth(&r, i))).unzip();
        // Well below the reference while it speaks, as a copy is.
        let below = |k: usize| rs[k] >= self.min_reference_db && rs[k] - ts[k] >= self.min_gap_db;
        // No louder than the copy would be: at most `max_excess_db` above the
        // reference less the copy's attenuation. Someone else talking over
        // the copy makes the track louder than that.
        let explained = |k: usize, offset: f32| t[k] - r[k] <= offset + self.max_excess_db;
        // Per frame: the best-matching lag's correlation and attenuation.
        let fits: Vec<Option<f32>> = (0..n)
            .map(|i| {
                if !below(i) {
                    return None;
                }
                let (c, lag) = (-lags..=lags)
                    .filter_map(|lag| {
                        correlation(&t, &r, i, half, lag, self.min_reference_db).map(|c| (c, lag))
                    })
                    .max_by(|a, b| a.0.total_cmp(&b.0))?;
                if c < self.min_correlation {
                    return None;
                }
                let offset = attenuation(&t, &r, i, half, lag, self.min_reference_db);
                explained(i, offset).then_some(offset)
            })
            .collect();
        // The hang covers a copy's onsets and tails, never a stretch the
        // copy does not explain.
        let hang = Self::frames(self.hang_ms);
        let mut out = vec![false; n];
        for (i, offset) in fits.iter().enumerate() {
            let Some(offset) = *offset else { continue };
            let from = i.saturating_sub(hang);
            let to = (i + hang + 1).min(n);
            for (k, copied) in out.iter_mut().enumerate().take(to).skip(from) {
                *copied |= below(k) && explained(k, offset);
            }
        }
        out
    }
}

/// Frames of reference speech a correlation needs (200 ms).
const MIN_SPEECH_FRAMES: usize = 20;

/// The `j` within `half` frames of `i` for which `j + lag` is a frame too.
fn window(n: usize, i: usize, half: usize, lag: isize) -> std::ops::Range<isize> {
    let n = n as isize;
    let from = (i as isize - half as isize).max(0).max(-lag);
    let to = (i as isize + half as isize + 1).min(n).min(n - lag);
    from..to.max(from)
}

/// The copy's attenuation around frame `i`: the 20th percentile of
/// `t[j] - r[j + lag]` where the reference speaks. A low percentile, because
/// where someone else talks over the copy the track is louder than the copy
/// alone, and those frames must not raise the estimate.
fn attenuation(t: &[f32], r: &[f32], i: usize, half: usize, lag: isize, floor: f32) -> f32 {
    let mut d: Vec<f32> = window(t.len(), i, half, lag)
        .filter(|&j| r[(j + lag) as usize] >= floor)
        .map(|j| t[j as usize] - r[(j + lag) as usize])
        .collect();
    if d.is_empty() {
        return f32::NEG_INFINITY;
    }
    let k = d.len() / 5;
    *d.select_nth_unstable_by(k, f32::total_cmp).1
}

/// Pearson correlation of `t[j]` and `r[j + lag]` over the `j` within `half`
/// frames of `i` where the reference speaks (`r[j + lag] >= floor`): a copy
/// follows the reference while it speaks, and stretches where both are
/// silent say nothing about it. `None` when either side is flat or the
/// reference spoke too little.
fn correlation(t: &[f32], r: &[f32], i: usize, half: usize, lag: isize, floor: f32) -> Option<f32> {
    let range = window(t.len(), i, half, lag);
    let (from, to) = (range.start, range.end);
    let mut count = 0usize;
    let (mut st, mut sr, mut stt, mut srr, mut str_) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for j in from..to {
        let b = r[(j + lag) as usize];
        if b < floor {
            continue;
        }
        count += 1;
        let a = f64::from(t[j as usize]);
        let b = f64::from(b);
        st += a;
        sr += b;
        stt += a * a;
        srr += b * b;
        str_ += a * b;
    }
    if count < MIN_SPEECH_FRAMES {
        return None;
    }
    let count = count as f64;
    let (mt, mr) = (st / count, sr / count);
    let cov = str_ / count - mt * mr;
    let vt = stt / count - mt * mt;
    let vr = srr / count - mr * mr;
    // A flat envelope (digital silence, a steady tone) correlates with nothing.
    if vt < 1e-3 || vr < 1e-3 {
        return None;
    }
    Some((cov / (vt * vr).sqrt()) as f32)
}

/// A track with every stretch that is only a copy of `reference` silenced.
pub struct BleedFilter<T: PcmSource, R: PcmSource> {
    track: T,
    reference: R,
    params: BleedParams,
    total: u64,
    /// Audio of both from `buf_start` on (the reference padded with zeros).
    track_buf: Vec<i16>,
    reference_buf: Vec<i16>,
    reference_ended: bool,
    buf_start: u64,
    /// Filtered samples not yet handed out, and the next one's position.
    ready: Vec<i16>,
    ready_pos: usize,
    emitted: u64,
    /// Samples silenced so far.
    removed: u64,
}

impl<T: PcmSource, R: PcmSource> BleedFilter<T, R> {
    /// `track`, read in step with `reference` (the other channel).
    pub fn new(track: T, reference: R, params: BleedParams) -> Self {
        let total = track.total();
        Self {
            track,
            reference,
            params,
            total,
            track_buf: Vec::new(),
            reference_buf: Vec::new(),
            reference_ended: false,
            buf_start: 0,
            ready: Vec::new(),
            ready_pos: 0,
            emitted: 0,
            removed: 0,
        }
    }

    /// Samples silenced so far.
    #[must_use]
    pub fn removed(&self) -> u64 {
        self.removed
    }

    /// Filter the next block into `ready`.
    fn next_block(&mut self) -> Result<(), String> {
        let margin = (self.params.margin() * FRAME) as u64;
        let block_end = (self.emitted + (BLOCK_FRAMES * FRAME) as u64).min(self.total);
        let want_end = (block_end + margin).min(self.total);
        // Drop what no later block reads.
        let keep_from = self.emitted.saturating_sub(margin).max(self.buf_start);
        let drop = (keep_from - self.buf_start) as usize;
        self.track_buf.drain(..drop.min(self.track_buf.len()));
        self.reference_buf
            .drain(..drop.min(self.reference_buf.len()));
        self.buf_start = keep_from;
        while self.buf_start + (self.track_buf.len() as u64) < want_end {
            let need = (want_end - self.buf_start - self.track_buf.len() as u64) as usize;
            let n = self.track.read(&mut self.track_buf, need.min(1 << 16))?;
            if n == 0 {
                return Err("the saved track ended before its recorded length".into());
            }
        }
        while self.reference_buf.len() < self.track_buf.len() {
            let need = self.track_buf.len() - self.reference_buf.len();
            let n = if self.reference_ended {
                0
            } else {
                self.reference
                    .read(&mut self.reference_buf, need.min(1 << 16))?
            };
            if n == 0 {
                // A shorter reference: nothing past its end is a copy.
                self.reference_ended = true;
                self.reference_buf.resize(self.track_buf.len(), 0);
            }
        }
        // The reference may have been read further than the track.
        let copied = self
            .params
            .copied_frames(&self.track_buf, &self.reference_buf[..self.track_buf.len()]);
        let from = (self.emitted - self.buf_start) as usize;
        let to = (block_end - self.buf_start) as usize;
        self.ready.clear();
        self.ready_pos = 0;
        self.ready.extend_from_slice(&self.track_buf[from..to]);
        for (k, sample) in self.ready.iter_mut().enumerate() {
            if copied[(from + k) / FRAME] {
                *sample = 0;
                self.removed += 1;
            }
        }
        self.emitted = block_end;
        Ok(())
    }
}

impl<T: PcmSource, R: PcmSource> PcmSource for BleedFilter<T, R> {
    fn total(&self) -> u64 {
        self.total
    }

    fn read(&mut self, out: &mut Vec<i16>, max: usize) -> Result<usize, String> {
        if self.ready_pos == self.ready.len() {
            if self.emitted == self.total {
                return Ok(0);
            }
            self.next_block()?;
        }
        let n = max.min(self.ready.len() - self.ready_pos);
        out.extend_from_slice(&self.ready[self.ready_pos..self.ready_pos + n]);
        self.ready_pos += n;
        Ok(n)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::windows::MemoryTrack;
    use super::*;

    /// A model-free stand-in for one person talking: noise shaped by a
    /// syllable-rate loudness envelope (80-300 ms syllables and pauses, each
    /// syllable made of 30-60 ms sounds of their own loudness), `seconds`
    /// long at peak `level`. Different seeds are different people.
    pub(crate) fn talker(seed: u64, seconds: f32, level: f32) -> Vec<i16> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32
        };
        let n = (seconds * 16_000.0) as usize;
        let mut envelope = Vec::with_capacity(n);
        while envelope.len() < n {
            let len = 1_280 + (next() * 3_520.0) as usize;
            let amp = if next() < 0.3 {
                0.0
            } else {
                0.2 + 0.8 * next()
            };
            let end = envelope.len() + len;
            while envelope.len() < end {
                let sound = 480 + (next() * 480.0) as usize;
                let loudness = amp * (0.3 + 0.7 * next());
                envelope.extend(std::iter::repeat_n(
                    loudness,
                    sound.min(end - envelope.len()),
                ));
            }
        }
        // Syllable edges smoothed over ~10 ms.
        let mut smooth = 0.0f32;
        (0..n)
            .map(|i| {
                smooth += (envelope[i] - smooth) / 160.0;
                ((next() * 2.0 - 1.0) * smooth * level) as i16
            })
            .collect()
    }

    /// `pcm` scaled by `gain` and delayed by `delay` samples.
    pub(crate) fn copy(pcm: &[i16], gain: f32, delay: usize) -> Vec<i16> {
        let mut out = vec![0i16; pcm.len()];
        for (o, &x) in out[delay..].iter_mut().zip(pcm) {
            *o = (f32::from(x) * gain) as i16;
        }
        out
    }

    pub(crate) fn mix(a: &[i16], b: &[i16]) -> Vec<i16> {
        a.iter()
            .zip(b)
            .map(|(&x, &y)| x.saturating_add(y))
            .collect()
    }

    fn filtered(track: &[i16], reference: &[i16], piece: usize) -> Vec<i16> {
        let mut filter = BleedFilter::new(
            MemoryTrack::new(track.to_vec()),
            MemoryTrack::new(reference.to_vec()),
            BleedParams::default(),
        );
        let mut out = Vec::new();
        while filter.read(&mut out, piece).unwrap() > 0 {}
        out
    }

    /// Of the samples where `of` is audible, the share `out` still carries.
    fn kept(out: &[i16], of: &[i16]) -> f32 {
        let audible: Vec<usize> = (0..of.len()).filter(|&i| of[i].abs() > 50).collect();
        audible.iter().filter(|&&i| out[i] != 0).count() as f32 / audible.len() as f32
    }

    #[test]
    fn a_quiet_delayed_copy_of_the_other_track_is_silenced_in_place() {
        // The person at the mic; the system track hears them 26 dB down and
        // 3 ms late, over a faint noise floor.
        let mic = talker(1, 30.0, 12_000.0);
        let system = mix(&copy(&mic, 0.05, 48), &talker(99, 30.0, 40.0));
        let out = filtered(&system, &mic, 4_096);
        assert_eq!(out.len(), system.len(), "times are unchanged");
        let left = kept(&out, &copy(&mic, 0.05, 48));
        assert!(left < 0.1, "{:.1}% of the copy is left", left * 100.0);
    }

    #[test]
    fn another_persons_quiet_speech_over_the_mic_speaker_is_kept() {
        // A remote person talking quietly while the person at the mic talks
        // loudly: no copy, so nothing is removed.
        let mic = talker(1, 30.0, 12_000.0);
        // Two people's loudness envelopes can rise and fall together for a
        // moment by chance; the bound allows for that, not for removing the
        // speaker (an attenuated copy loses over 90%, above).
        let remote = talker(2, 30.0, 600.0);
        let left = kept(&filtered(&remote, &mic, 4_096), &remote);
        assert!(
            left > 0.99,
            "only {:.1}% of the remote speaker is left",
            left * 100.0
        );
    }

    #[test]
    fn another_persons_speech_on_top_of_the_copy_is_kept() {
        // Overlap: the copy and a different remote speaker at once. Wherever
        // the remote speaker is louder than the copy (by 3 dB), they
        // are kept. Sounds about as faint as the copy or fainter go with it:
        // this silences, it does not subtract.
        let mic = talker(1, 30.0, 12_000.0);
        let remote = talker(3, 30.0, 3_000.0);
        let bleed = copy(&mic, 0.05, 48);
        let system = mix(&bleed, &remote);
        let out = filtered(&system, &mic, 4_096);
        let power = |x: &[i16]| x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>();
        let (mut audible, mut left) = (0usize, 0usize);
        for ((r, b), o) in remote
            .chunks(FRAME)
            .zip(bleed.chunks(FRAME))
            .zip(out.chunks(FRAME))
        {
            if power(r) >= 2.0 * power(b) && power(r) > 0.0 {
                audible += 1;
                left += usize::from(o.iter().any(|&x| x != 0));
            }
        }
        let share = left as f32 / audible as f32;
        assert!(
            share > 0.99,
            "only {:.1}% of the remote speaker's frames are left",
            share * 100.0
        );
    }

    #[test]
    fn nothing_is_removed_while_the_other_track_is_silent() {
        let system = talker(4, 20.0, 8_000.0);
        assert_eq!(filtered(&system, &vec![0; system.len()], 4_096), system);
        // A reference shorter than the track is silent past its end.
        assert_eq!(filtered(&system, &[], 4_096), system);
    }

    #[test]
    fn the_output_is_the_same_however_the_track_is_read() {
        let mic = talker(5, 25.0, 12_000.0);
        let system = mix(&copy(&mic, 0.05, 48), &talker(6, 25.0, 3_000.0));
        let whole = filtered(&system, &mic, 1 << 20);
        assert_eq!(filtered(&system, &mic, 7), whole);
        assert_eq!(filtered(&system, &mic, 160_001), whole);
        // And it is the whole-track decision, frame for frame.
        let copied = BleedParams::default().copied_frames(&system, &mic);
        for (i, (&x, &y)) in system.iter().zip(&whole).enumerate() {
            assert_eq!(y, if copied[i / FRAME] { 0 } else { x }, "sample {i}");
        }
        assert!(copied.iter().any(|&c| c) && !copied.iter().all(|&c| c));
    }
}
