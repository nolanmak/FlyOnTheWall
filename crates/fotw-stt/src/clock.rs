//! Rebasing provider timestamps onto our session clock (spec 7.2 rule 1).
//!
//! Providers report time relative to *their* connection, starting at zero.
//! Everything above this crate — seek, citations, export, the WAL — works in
//! milliseconds from session t0 on our own monotonic audio clock. The offset
//! between the two is owned here, and recomputed on every reconnect so a
//! dropped socket does not fold the second half of a meeting back onto the
//! first.

use crate::TimestampSource;

/// Convert a provider's floating-point seconds into our integer milliseconds.
///
/// Deepgram (and ElevenLabs, and every batch API) reports `start`/`end` in
/// seconds. Rounds to nearest rather than truncating: truncation biases every
/// timestamp early, and across a two-hour meeting's worth of words that bias is
/// systematic rather than noise.
///
/// Total by construction — a provider cannot make this panic. Negative and
/// `NaN` inputs clamp to `0`, and `+inf` saturates, which is Rust's defined
/// float-to-integer cast behaviour rather than anything we add.
#[must_use]
pub fn seconds_to_ms(seconds: f64) -> u64 {
    // `as` on a float→int cast saturates at the bounds and maps NaN to 0.
    (seconds * 1_000.0).round() as u64
}

/// The offset between one provider connection's clock and our session clock.
///
/// Cheap to copy; adapters keep one per stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionClock {
    /// Session-clock milliseconds corresponding to this connection's zero.
    offset_ms: u64,
}

impl SessionClock {
    /// A clock for a connection opened at session t0.
    #[must_use]
    pub fn new() -> Self {
        Self { offset_ms: 0 }
    }

    /// A clock for a connection whose first audio sample is at `session_ms`.
    #[must_use]
    pub fn opened_at(session_ms: u64) -> Self {
        Self {
            offset_ms: session_ms,
        }
    }

    /// Recompute the offset after a reconnect.
    ///
    /// `session_ms` is the session-clock position of the **first audio sample
    /// fed to the new connection**, not the moment the socket opened. With
    /// STT-09's gapless replay those differ by the length of the replayed ring,
    /// and using the wrong one is exactly the off-by-30-seconds this method
    /// exists to prevent.
    pub fn reconnected_at(&mut self, session_ms: u64) {
        self.offset_ms = session_ms;
    }

    /// Session-clock milliseconds corresponding to this connection's zero.
    #[must_use]
    pub fn offset_ms(&self) -> u64 {
        self.offset_ms
    }

    /// Rebase a provider-relative millisecond onto the session clock.
    #[must_use]
    pub fn to_session_ms(&self, provider_relative_ms: u64) -> u64 {
        self.offset_ms.saturating_add(provider_relative_ms)
    }

    /// Rebase provider-relative seconds straight onto the session clock.
    #[must_use]
    pub fn seconds_to_session_ms(&self, provider_relative_seconds: f64) -> u64 {
        self.to_session_ms(seconds_to_ms(provider_relative_seconds))
    }
}

/// A time range on the session clock, carrying how it was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSpan {
    /// Milliseconds from session t0.
    pub start_ms: u64,
    /// Milliseconds from session t0.
    pub end_ms: u64,
    /// Whether the provider reported these times or we synthesized them.
    pub source: TimestampSource,
}

impl TimeSpan {
    /// A span the provider reported, rebased onto our clock.
    ///
    /// `end` is clamped up to `start`: a provider that reports an inverted range
    /// should not be able to produce a segment whose duration underflows.
    #[must_use]
    pub fn provided(start_ms: u64, end_ms: u64) -> Self {
        Self {
            start_ms,
            end_ms: end_ms.max(start_ms),
            source: TimestampSource::Provider,
        }
    }

    /// A span we synthesized from our own audio-clock position.
    #[must_use]
    pub fn estimated(start_ms: u64, end_ms: u64) -> Self {
        Self {
            start_ms,
            end_ms: end_ms.max(start_ms),
            source: TimestampSource::Estimated,
        }
    }

    /// Duration in milliseconds.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.end_ms - self.start_ms
    }
}

/// Synthesizes timestamps for providers that report none (spec 7.2 rule 3).
///
/// OpenAI's realtime transcription session returns text with no word timings and
/// no segment timings. The only clock we have is our own: the audio-clock
/// position at the moment the delta arrived. That is genuinely an estimate — it
/// includes the provider's queueing and network latency — so everything this
/// produces is stamped [`TimestampSource::Estimated`] and the UI greys out
/// click-to-seek accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArrivalEstimator {
    last_end_ms: u64,
}

impl ArrivalEstimator {
    /// An estimator anchored at session t0.
    #[must_use]
    pub fn new() -> Self {
        Self { last_end_ms: 0 }
    }

    /// An estimator anchored at an arbitrary session position.
    #[must_use]
    pub fn starting_at(session_ms: u64) -> Self {
        Self {
            last_end_ms: session_ms,
        }
    }

    /// Where the next synthesized span will start.
    #[must_use]
    pub fn last_end_ms(&self) -> u64 {
        self.last_end_ms
    }

    /// Synthesize the span for a delta that arrived at `arrival_session_ms`.
    ///
    /// The span runs from the end of the previous one to the arrival position,
    /// so consecutive spans tile the timeline without gaps or overlap. An
    /// arrival that appears to move backwards (clock jitter, a reordered delta)
    /// yields a zero-length span at the current position rather than an
    /// inverted one.
    pub fn next_span(&mut self, arrival_session_ms: u64) -> TimeSpan {
        let start_ms = self.last_end_ms;
        let end_ms = arrival_session_ms.max(start_ms);
        self.last_end_ms = end_ms;
        TimeSpan::estimated(start_ms, end_ms)
    }

    /// Move the anchor forward to a known-good session position.
    ///
    /// Called whenever the provider *did* supply a timestamp, so that if it
    /// later stops supplying them the synthesized spans resume from real time
    /// rather than from the last thing we guessed. Never moves backwards.
    pub fn advance_to(&mut self, session_ms: u64) {
        self.last_end_ms = self.last_end_ms.max(session_ms);
    }

    /// Re-anchor after a reconnect so synthesized spans stay continuous.
    pub fn reconnected_at(&mut self, session_ms: u64) {
        self.advance_to(session_ms);
    }
}
