//! What the post-pass asks of the speech engines, so the job can run against
//! the helper in production and a counting fake in tests.

use futures_util::future::BoxFuture;

use super::merge::TimedWord;
use super::turns::SpeakerSpan;

/// Speech detection, transcription and diarization over saved audio.
pub trait PassEngine: Send + Sync {
    /// Speech in `pcm` (mono s16 at 16 kHz) as `[start, end)` samples.
    fn speech<'a>(&'a self, pcm: &'a [i16]) -> BoxFuture<'a, Result<Vec<(u64, u64)>, String>>;

    /// Words in `pcm`, timed in ms from its first sample.
    fn transcribe<'a>(&'a self, pcm: &'a [i16]) -> BoxFuture<'a, Result<Vec<TimedWord>, String>>;

    /// A diarization of one channel, or `None` when no diarizer is available
    /// (the cleaned transcript then has no remote speaker labels).
    fn diarize(&self) -> BoxFuture<'_, Result<Option<Box<dyn Diarization>>, String>>;

    /// Engines and revisions, recorded with the cleaned version.
    fn provenance(&self) -> serde_json::Value;
}

/// One channel's diarization: the whole channel in order, then its speakers.
pub trait Diarization: Send {
    /// More of the channel. May wait for the engine to catch up.
    fn push<'a>(&'a mut self, pcm: &'a [i16]) -> BoxFuture<'a, Result<(), String>>;

    /// The channel's speakers, sorted by start; labels are stable within it.
    fn finish(self: Box<Self>) -> BoxFuture<'static, Result<Vec<SpeakerSpan>, String>>;
}

#[cfg(test)]
pub(crate) mod fake {
    //! Model-free stand-ins with the same rules as the helper's `fake-vad`,
    //! `fake-words` and `fake-diarizer` (docs/SPEECHD_PROTOCOL.md): a 100 ms
    //! block of one repeated nonzero sample `v` is the word `w<v>`, runs of
    //! equal blocks are one word, and a block's loudness picks its speaker.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const BLOCK: usize = 1_600;

    /// Counts finished transcriptions; can hang on a chosen call, like a
    /// daemon killed with a window in flight.
    #[derive(Default)]
    pub(crate) struct FakeEngine {
        /// Transcriptions that returned.
        pub transcribed: Arc<AtomicUsize>,
        /// Calls made, finished or not.
        pub started: Arc<AtomicUsize>,
        /// Hang forever on this call (1-based), if set.
        pub hang_on: Option<usize>,
        /// Report no diarizer.
        pub no_diarizer: bool,
    }

    pub(crate) fn words(pcm: &[i16]) -> Vec<TimedWord> {
        let mut out: Vec<TimedWord> = Vec::new();
        let mut last: Option<(usize, i16)> = None;
        for (i, block) in pcm.chunks_exact(BLOCK).enumerate() {
            let v = block[0];
            let word = v != 0 && block.iter().all(|&x| x == v);
            let (start, end) = ((i * 100) as u64, (i * 100 + 100) as u64);
            match (word, last) {
                (true, Some((j, lv))) if lv == v && j + 1 == i => {
                    out.last_mut().expect("a word").end_ms = end;
                }
                (true, _) => out.push(TimedWord::new(format!("w{v}"), start, end)),
                _ => {}
            }
            last = word.then_some((i, v));
        }
        out
    }

    impl PassEngine for FakeEngine {
        fn speech<'a>(&'a self, pcm: &'a [i16]) -> BoxFuture<'a, Result<Vec<(u64, u64)>, String>> {
            Box::pin(async move {
                let mut out: Vec<(u64, u64)> = Vec::new();
                for (i, block) in pcm.chunks(BLOCK).enumerate() {
                    if block.iter().any(|&x| x != 0) {
                        let (s, e) = ((i * BLOCK) as u64, (i * BLOCK + block.len()) as u64);
                        match out.last_mut() {
                            Some(last) if last.1 == s => last.1 = e,
                            _ => out.push((s, e)),
                        }
                    }
                }
                Ok(out)
            })
        }

        fn transcribe<'a>(
            &'a self,
            pcm: &'a [i16],
        ) -> BoxFuture<'a, Result<Vec<TimedWord>, String>> {
            Box::pin(async move {
                let call = self.started.fetch_add(1, Ordering::SeqCst) + 1;
                if self.hang_on == Some(call) {
                    std::future::pending::<()>().await;
                }
                tokio::task::yield_now().await;
                self.transcribed.fetch_add(1, Ordering::SeqCst);
                Ok(words(pcm))
            })
        }

        fn diarize(&self) -> BoxFuture<'_, Result<Option<Box<dyn Diarization>>, String>> {
            let none = self.no_diarizer;
            Box::pin(async move {
                Ok((!none).then(|| Box::new(FakeDiarization::default()) as Box<dyn Diarization>))
            })
        }

        fn provenance(&self) -> serde_json::Value {
            serde_json::json!({"asr": "fake-words@fake", "vad": "fake-vad@fake", "diarizer": "fake-diarizer@fake"})
        }
    }

    /// Per 100 ms block: quiet is silence, peak below 16384 one speaker, at
    /// or above it another; labelled by first appearance.
    #[derive(Default)]
    pub(crate) struct FakeDiarization {
        pending: Vec<i16>,
        block: u64,
        classes: Vec<bool>,
        spans: Vec<(u64, u64, bool)>,
    }

    impl FakeDiarization {
        fn take_blocks(&mut self, all: bool) {
            let whole = if all {
                self.pending.len()
            } else {
                self.pending.len() / BLOCK * BLOCK
            };
            let taken: Vec<i16> = self.pending.drain(..whole).collect();
            for block in taken.chunks(BLOCK) {
                let peak = block.iter().map(|x| x.unsigned_abs()).max().unwrap_or(0);
                let (s, e) = (self.block * 100, self.block * 100 + 100);
                self.block += 1;
                if peak < 328 {
                    continue;
                }
                let loud = peak >= 16_384;
                if !self.classes.contains(&loud) {
                    self.classes.push(loud);
                }
                match self.spans.last_mut() {
                    Some(last) if last.1 == s && last.2 == loud => last.1 = e,
                    _ => self.spans.push((s, e, loud)),
                }
            }
        }
    }

    impl Diarization for FakeDiarization {
        fn push<'a>(&'a mut self, pcm: &'a [i16]) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.pending.extend_from_slice(pcm);
                self.take_blocks(false);
                Ok(())
            })
        }

        fn finish(mut self: Box<Self>) -> BoxFuture<'static, Result<Vec<SpeakerSpan>, String>> {
            Box::pin(async move {
                self.take_blocks(true);
                let classes = self.classes.clone();
                Ok(self
                    .spans
                    .iter()
                    .map(|&(s, e, loud)| SpeakerSpan {
                        start_ms: s,
                        end_ms: e,
                        speaker: format!(
                            "S{}",
                            classes.iter().position(|&c| c == loud).unwrap_or(0) + 1
                        ),
                    })
                    .collect())
            })
        }
    }
}
