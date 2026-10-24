//! The lock-free handoff between the audio callback and the pump thread.
//!
//! Seam-adjacent but deliberately not in `fotw-audio`: the ring is pipeline
//! policy, not platform knowledge.
//!
//! The producer half runs on a real-time thread and may only `memcpy` and bump
//! an atomic. It **never blocks**. On overrun it drops the shortfall and
//! counts it, because stalling the audio thread to wait for a slow consumer is
//! the one failure this architecture cannot tolerate — and a dropped block is
//! recoverable anyway, since the write-ahead log is written independently of
//! the network path (docs/REQUIREMENTS.md 5.2, 5.3).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

/// Constructs a producer/consumer pair over a preallocated ring.
#[derive(Debug)]
pub struct AudioRing;

impl AudioRing {
    /// Allocate a ring holding `frames` samples and split it.
    ///
    /// All allocation happens here, before any real-time section begins.
    #[must_use]
    pub fn with_capacity_frames(frames: usize) -> (RingProducer, RingConsumer) {
        let (producer, consumer) = RingBuffer::<f32>::new(frames);
        let dropped = Arc::new(AtomicU64::new(0));
        (
            RingProducer {
                inner: producer,
                dropped: Arc::clone(&dropped),
            },
            RingConsumer {
                inner: consumer,
                dropped,
            },
        )
    }
}

/// The real-time half. Writes only; never blocks, never allocates.
#[derive(Debug)]
pub struct RingProducer {
    inner: Producer<f32>,
    dropped: Arc<AtomicU64>,
}

impl RingProducer {
    /// Copy as much of `block` as fits, returning how many samples were taken.
    ///
    /// A short write is not an error: it means the consumer is behind, and the
    /// shortfall is added to [`RingProducer::dropped_frames`] for the pump to
    /// surface. Callers on the audio thread must ignore the return value
    /// rather than retry — retrying is blocking by another name.
    pub fn push_block(&mut self, block: &[f32]) -> usize {
        // `push_partial_slice` copies straight into the ring's own storage with
        // no intermediate buffer and no allocation. It returns
        // (what was taken, what did not fit).
        let (pushed, remainder) = self.inner.push_partial_slice(block);
        let took = pushed.len();
        if !remainder.is_empty() {
            self.dropped
                .fetch_add(remainder.len() as u64, Ordering::Relaxed);
        }
        took
    }

    /// Copy a complete callback or drop/count the entire callback.
    ///
    /// Capture callbacks contain complete interleaved frames. A partial sample
    /// write can split a frame and shift all later channels, so capture sinks
    /// use this operation even before startup reports the channel count.
    /// The single producer owns all writes; a concurrent consumer can only free
    /// more slots between the check and copy. No allocation, lock or retry.
    pub fn push_complete_block(&mut self, block: &[f32]) -> usize {
        if self.inner.slots() < block.len() {
            self.dropped
                .fetch_add(block.len() as u64, Ordering::Relaxed);
            return 0;
        }
        self.push_block(block)
    }

    /// Samples dropped because the consumer could not keep up.
    ///
    /// Non-zero here is the signal that the pump is falling behind. It must be
    /// surfaced, not swallowed: silent drops are exactly the class of failure
    /// this project is organised around avoiding.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Free space, in samples.
    #[must_use]
    pub fn slots(&self) -> usize {
        self.inner.slots()
    }

    /// True when the consumer has been dropped.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.inner.is_abandoned()
    }
}

/// The pump half. Drains on a normal-priority thread.
#[derive(Debug)]
pub struct RingConsumer {
    inner: Consumer<f32>,
    dropped: Arc<AtomicU64>,
}

impl RingConsumer {
    /// Drain up to `out.len()` samples, returning how many were written.
    pub fn pop_into(&mut self, out: &mut [f32]) -> usize {
        let (popped, _unfilled) = self.inner.pop_partial_slice(out);
        popped.len()
    }

    /// Samples the producer dropped because this half could not keep up.
    ///
    /// The same counter [`RingProducer::dropped_frames`] reads, from the side
    /// that can actually report it: the producer moves onto the audio thread
    /// and is never seen again, so a pump that only holds a consumer had no
    /// way to read its own shortfall — which is why ring drops were invisible
    /// for the life of the project (#79).
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Samples ready to read.
    #[must_use]
    pub fn slots(&self) -> usize {
        self.inner.slots()
    }

    /// True when the producer has been dropped.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.inner.is_abandoned()
    }
}

/// Control-thread factory for isolated tap generations. A replacement's PCM
/// must not enter the old consumer while the pump is draining/padding its gap.
/// At most one pending consumer is retained; a failed start is superseded by
/// the next attempt. No mutex or allocation is used by the returned producer.
#[derive(Clone)]
pub struct GenerationRing {
    state: Arc<std::sync::Mutex<GenerationState>>,
    capacity: usize,
}
struct GenerationState {
    initial: Option<RingProducer>,
    next: Option<RingConsumer>,
}
impl GenerationRing {
    /// Keep the initial ring; allocate each replacement on the control thread.
    pub fn new(initial: RingProducer) -> Self {
        let capacity = initial.inner.buffer().capacity();
        Self {
            state: Arc::new(std::sync::Mutex::new(GenerationState {
                initial: Some(initial),
                next: None,
            })),
            capacity,
        }
    }
    /// Build a producer for the next attempt, isolating it from earlier PCM.
    pub fn producer(&self) -> RingProducer {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(initial) = state.initial.take() {
            return initial;
        }
        let (producer, consumer) = AudioRing::with_capacity_frames(self.capacity);
        state.next = Some(consumer);
        producer
    }
    /// Consume the most recent attempt after its authoritative format is known.
    pub fn take_replacement(&self) -> Option<RingConsumer> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next
            .take()
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    #[test]
    fn retained_old_producer_and_failed_attempt_cannot_mix_with_replacement() {
        let (producer, mut old_consumer) = AudioRing::with_capacity_frames(8);
        let generations = GenerationRing::new(producer);
        let mut old = generations.producer();
        old.push_block(&[1.0, 1.0]);
        let mut failed_attempt = generations.producer();
        failed_attempt.push_block(&[2.0]);
        let mut replacement = generations.producer();
        replacement.push_block(&[3.0, 3.0]);
        assert!(failed_attempt.is_abandoned());
        old.push_block(&[1.0]);
        let mut current = generations.take_replacement().unwrap();
        let mut buffer = [0.0; 8];
        assert_eq!(old_consumer.pop_into(&mut buffer), 3);
        assert_eq!(&buffer[..3], &[1.0, 1.0, 1.0]);
        assert_eq!(current.pop_into(&mut buffer), 2);
        assert_eq!(&buffer[..2], &[3.0, 3.0]);
        assert!(generations.take_replacement().is_none());
    }
}
