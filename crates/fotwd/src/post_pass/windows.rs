//! Reading a saved track one bounded window at a time (#246).
//!
//! Ported in spirit from Muesli (https://github.com/Muesli-HQ/muesli, MIT,
//! © 2026 Pranav Hari), native/MuesliNative/Sources/MuesliNativeApp/MeetingRecordingTranscriber.swift
//! (`RecordingAudioWindowReader`) at commit
//! 97e455794112126a896cae17f5c829e8ffa820c8: only one window of audio exists
//! at a time, and the caller finishes inference before asking for the next,
//! which is the backpressure. Here the source is the archived Ogg Opus track,
//! decoded a packet at a time, and windows are 30 s with 2 s of overlap.

use std::path::Path;

use fotw_pipeline::opus::OpusStreamReader;

/// Sample rate the helper's engines take.
pub const RATE: u32 = 16_000;
/// Window length.
pub const WINDOW_MS: u64 = 30_000;
/// Shared audio between consecutive windows.
pub const OVERLAP_MS: u64 = 2_000;

/// Samples in `ms` at [`RATE`].
#[must_use]
pub fn samples(ms: u64) -> u64 {
    ms * u64::from(RATE) / 1_000
}

/// Milliseconds in `samples` at [`RATE`].
#[must_use]
pub fn millis(samples: u64) -> u64 {
    samples * 1_000 / u64::from(RATE)
}

/// Every window of a track `total` samples long, as `[start, end)` samples.
///
/// The last window ends at the track's end; no window lies wholly inside the
/// previous one's overlap.
#[must_use]
pub fn plan(total: u64, window: u64, overlap: u64) -> Vec<(u64, u64)> {
    assert!(overlap < window, "overlap must be shorter than a window");
    let mut out = Vec::new();
    if total == 0 {
        return out;
    }
    let mut start = 0;
    loop {
        let end = (start + window).min(total);
        out.push((start, end));
        if end == total {
            return out;
        }
        start = end - overlap;
    }
}

/// One window's audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmWindow {
    /// Its position in [`plan`].
    pub index: usize,
    /// First sample, from the track's start.
    pub start: u64,
    /// Mono s16 at [`RATE`].
    pub pcm: Vec<i16>,
}

/// Where a reader's audio comes from.
pub trait PcmSource: Send {
    /// Samples in the track.
    fn total(&self) -> u64;
    /// Append up to `max` samples to `out`; `0` at the end.
    ///
    /// # Errors
    ///
    /// A decode failure.
    fn read(&mut self, out: &mut Vec<i16>, max: usize) -> Result<usize, String>;
}

impl PcmSource for Box<dyn PcmSource> {
    fn total(&self) -> u64 {
        self.as_ref().total()
    }

    fn read(&mut self, out: &mut Vec<i16>, max: usize) -> Result<usize, String> {
        self.as_mut().read(out, max)
    }
}

/// An archived Opus track.
pub struct OpusTrack {
    reader: OpusStreamReader<std::fs::File>,
    total: u64,
    scratch: Vec<f32>,
}

impl OpusTrack {
    /// Open `path`.
    ///
    /// # Errors
    ///
    /// When the file is missing or is not an Ogg Opus track.
    pub fn open(path: &Path) -> Result<Self, String> {
        let reader = OpusStreamReader::open(path, RATE).map_err(|e| e.to_string())?;
        let total = reader
            .total_samples()
            .ok_or_else(|| "the saved track has no end position".to_owned())?;
        Ok(Self {
            reader,
            total,
            scratch: Vec::new(),
        })
    }
}

impl PcmSource for OpusTrack {
    fn total(&self) -> u64 {
        self.total
    }

    fn read(&mut self, out: &mut Vec<i16>, max: usize) -> Result<usize, String> {
        self.scratch.clear();
        let n = self
            .reader
            .read_into(&mut self.scratch, max)
            .map_err(|e| e.to_string())?;
        out.extend(self.scratch.iter().map(|&x| to_i16(x)));
        Ok(n)
    }
}

/// In-memory PCM, for tests and fixtures.
pub struct MemoryTrack {
    pcm: Vec<i16>,
    pos: usize,
}

impl MemoryTrack {
    /// A track holding `pcm`.
    #[must_use]
    pub fn new(pcm: Vec<i16>) -> Self {
        Self { pcm, pos: 0 }
    }
}

impl PcmSource for MemoryTrack {
    fn total(&self) -> u64 {
        self.pcm.len() as u64
    }

    fn read(&mut self, out: &mut Vec<i16>, max: usize) -> Result<usize, String> {
        let n = max.min(self.pcm.len() - self.pos);
        out.extend_from_slice(&self.pcm[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Yields a track's windows in order, holding at most one window of audio.
pub struct WindowReader<S: PcmSource> {
    source: S,
    plan: Vec<(u64, u64)>,
    next: usize,
    /// Audio from `buf_start` on.
    buf: Vec<i16>,
    buf_start: u64,
}

impl<S: PcmSource> WindowReader<S> {
    /// Windows of [`WINDOW_MS`] with [`OVERLAP_MS`] of overlap.
    pub fn new(source: S) -> Self {
        Self::with_sizes(source, samples(WINDOW_MS), samples(OVERLAP_MS))
    }

    /// Windows of `window` samples overlapping by `overlap`.
    pub fn with_sizes(source: S, window: u64, overlap: u64) -> Self {
        let plan = plan(source.total(), window, overlap);
        Self {
            source,
            plan,
            next: 0,
            buf: Vec::new(),
            buf_start: 0,
        }
    }

    /// The whole plan, as `[start, end)` samples.
    #[must_use]
    pub fn plan(&self) -> &[(u64, u64)] {
        &self.plan
    }

    /// The next window, or `None` after the last. Decodes the windows a
    /// resumed job skips too, because a compressed track can only be read in
    /// order; `skip(i)` says not to copy them out.
    ///
    /// # Errors
    ///
    /// A decode failure, or a track shorter than its recorded length.
    pub fn next_window(&mut self, skip: impl Fn(usize) -> bool) -> Result<Option<PcmWindow>, String> {
        let Some(&(start, end)) = self.plan.get(self.next) else {
            return Ok(None);
        };
        let index = self.next;
        self.next += 1;
        // Drop audio before this window.
        let drop = (start - self.buf_start).min(self.buf.len() as u64) as usize;
        self.buf.drain(..drop);
        self.buf_start += drop as u64;
        while self.buf_start + (self.buf.len() as u64) < end {
            let want = (end - self.buf_start - self.buf.len() as u64) as usize;
            // Skip audio before `start` that an earlier skip left unread.
            let n = self.source.read(&mut self.buf, want.min(1 << 16))?;
            if n == 0 {
                return Err("the saved track ended before its recorded length".into());
            }
            if self.buf_start < start {
                let drop = (start - self.buf_start).min(self.buf.len() as u64) as usize;
                self.buf.drain(..drop);
                self.buf_start += drop as u64;
            }
        }
        let from = (start - self.buf_start) as usize;
        let to = (end - self.buf_start) as usize;
        let pcm = if skip(index) {
            Vec::new()
        } else {
            self.buf[from..to].to_vec()
        };
        Ok(Some(PcmWindow { index, start, pcm }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_thirty_seconds_overlapping_by_two_and_cover_the_track() {
        let total = samples(95_000);
        let plan = plan(total, samples(WINDOW_MS), samples(OVERLAP_MS));
        let ms: Vec<(u64, u64)> = plan.iter().map(|&(s, e)| (millis(s), millis(e))).collect();
        assert_eq!(
            ms,
            [(0, 30_000), (28_000, 58_000), (56_000, 86_000), (84_000, 95_000)]
        );
        assert!(super::plan(0, 10, 2).is_empty());
        // A track that ends inside an overlap gets no window of overlap alone.
        assert_eq!(super::plan(30, 30, 2), [(0, 30)]);
        assert_eq!(super::plan(31, 30, 2), [(0, 30), (28, 31)]);
    }

    #[test]
    fn the_reader_hands_out_each_windows_exact_samples_and_skips_cheaply() {
        let pcm: Vec<i16> = (0..1_000).map(|i| i as i16).collect();
        let mut reader = WindowReader::with_sizes(MemoryTrack::new(pcm.clone()), 300, 40);
        let mut seen = Vec::new();
        while let Some(w) = reader.next_window(|i| i == 1).unwrap() {
            seen.push((w.index, w.start, w.pcm.len()));
            if w.index != 1 {
                assert_eq!(w.pcm[..], pcm[w.start as usize..w.start as usize + w.pcm.len()]);
            }
        }
        assert_eq!(
            seen,
            [(0, 0, 300), (1, 260, 0), (2, 520, 300), (3, 780, 220)]
        );
    }
}
