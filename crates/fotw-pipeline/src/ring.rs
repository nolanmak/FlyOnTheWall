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

/// Latest callback timing mapped to this generation's accepted PCM samples.
///
/// This is a monitoring snapshot, not a timestamp for an arbitrary pump chunk.
/// Intermediate callbacks may be superseded. Host stamps describe the tap's
/// clock contract (native input time or a flagged fallback on macOS), not proof
/// of hardware playback or acoustic alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureObservation {
    /// Nonempty callback number within this ring generation, starting at one.
    pub sequence: u64,
    /// Accepted interleaved samples preceding this callback, including untimed writes.
    pub sample_start: u64,
    /// Accepted samples in this callback; zero means the whole callback was dropped.
    pub sample_count: usize,
    /// Device frame index of the callback's first frame, before ring admission.
    pub device_frames: u64,
    /// Host nanoseconds reported by the capture backend, without reinterpretation.
    pub host_ns: u64,
    /// Original capture flag bits, including discontinuity and timestamp error.
    pub flags: u32,
    /// Cumulative samples rejected by this ring, including this callback.
    pub dropped_samples: u64,
    /// Number of callbacks carrying discontinuity/timestamp-error flags in this
    /// generation. Unlike latest flags, this evidence survives supersession.
    pub timing_faults: u64,
}

/// One writer owned by RingProducer. All fields are atomic so even an interrupted
/// publication has no data race. Sequential consistency makes a matching even
/// version enclose exactly one publication. Readers make one attempt, never spin.
#[derive(Debug, Default)]
struct ClockMailbox {
    version: AtomicU64,
    fields: [AtomicU64; 8],
}

impl ClockMailbox {
    fn publish(&self, observation: CaptureObservation) {
        let end = observation.sequence.wrapping_mul(2);
        self.version.store(end.wrapping_sub(1), Ordering::SeqCst);
        for (field, value) in self.fields.iter().zip([
            observation.sample_start,
            observation.sample_count as u64,
            observation.device_frames,
            observation.host_ns,
            u64::from(observation.flags),
            observation.dropped_samples,
            observation.sequence,
            observation.timing_faults,
        ]) {
            field.store(value, Ordering::SeqCst);
        }
        self.version.store(end, Ordering::SeqCst);
    }

    fn read(&self) -> Option<CaptureObservation> {
        let before = self.version.load(Ordering::SeqCst);
        if before == 0 || !before.is_multiple_of(2) {
            return None;
        }
        let values = self
            .fields
            .each_ref()
            .map(|field| field.load(Ordering::SeqCst));
        if self.version.load(Ordering::SeqCst) != before {
            return None;
        }
        Some(CaptureObservation {
            sample_start: values[0],
            sample_count: values[1] as usize,
            device_frames: values[2],
            host_ns: values[3],
            flags: values[4] as u32,
            dropped_samples: values[5],
            sequence: values[6],
            timing_faults: values[7],
        })
    }
}

impl AudioRing {
    /// Allocate a ring holding `frames` samples and split it.
    ///
    /// All allocation happens here, before any real-time section begins.
    #[must_use]
    pub fn with_capacity_frames(frames: usize) -> (RingProducer, RingConsumer) {
        let (producer, consumer) = RingBuffer::<f32>::new(frames);
        let dropped = Arc::new(AtomicU64::new(0));
        let clock = Arc::new(ClockMailbox::default());
        (
            RingProducer {
                inner: producer,
                dropped: Arc::clone(&dropped),
                clock: Arc::clone(&clock),
                accepted_samples: 0,
                clock_sequence: 0,
                timing_faults: 0,
            },
            RingConsumer {
                inner: consumer,
                dropped,
                clock,
            },
        )
    }
}

/// The real-time half. Writes only; never blocks, never allocates.
#[derive(Debug)]
pub struct RingProducer {
    inner: Producer<f32>,
    dropped: Arc<AtomicU64>,
    clock: Arc<ClockMailbox>,
    accepted_samples: u64,
    clock_sequence: u64,
    timing_faults: u64,
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
        self.accepted_samples = self.accepted_samples.saturating_add(took as u64);
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

    /// Admit a whole capture callback and publish its timing without allocating,
    /// locking or retrying. An overrun publishes a zero-length observation so a
    /// reader cannot mistake a rejected buffer for continuous accepted audio.
    /// Empty callbacks do not replace the latest meaningful observation.
    pub fn push_capture_block(
        &mut self,
        block: &[f32],
        device_frames: u64,
        host_ns: u64,
        flags: u32,
    ) -> usize {
        let sample_start = self.accepted_samples;
        let sample_count = self.push_complete_block(block);
        if !block.is_empty() {
            self.clock_sequence = self.clock_sequence.wrapping_add(1);
            // FrameFlags' stable WASAPI-compatible bits: discontinuity=2,
            // timestamp error=4. SILENT=1 is not a timing fault. Keep the seam's
            // raw representation without importing platform capture into DSP.
            if flags & 0b110 != 0 {
                self.timing_faults = self.timing_faults.saturating_add(1);
            }
            self.clock.publish(CaptureObservation {
                sequence: self.clock_sequence,
                sample_start,
                sample_count,
                device_frames,
                host_ns,
                flags,
                dropped_samples: self.dropped_frames(),
                timing_faults: self.timing_faults,
            });
        }
        sample_count
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
    clock: Arc<ClockMailbox>,
}

impl RingConsumer {
    /// Read one coherent latest capture observation. None means no observation
    /// yet or a publication in progress; callers must defer rather than spin.
    /// The snapshot can precede or follow the PCM drained by `pop_into`; use its
    /// absolute sample positions, never attach it directly to that drain.
    #[must_use]
    pub fn capture_observation(&self) -> Option<CaptureObservation> {
        self.clock.read()
    }
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
    /// Abandon an unsuccessful initial attempt, including PCM emitted before
    /// start returned an error. Replace the caller's consumer with this empty
    /// one; every later producer will have a separate replacement consumer,
    /// even when the initial open failed before requesting a producer.
    pub fn discard_initial_attempt(&self) -> RingConsumer {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.initial.take();
        state.next.take();
        let (_producer, consumer) = AudioRing::with_capacity_frames(self.capacity);
        consumer
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
    fn interrupted_clock_publication_defers_instead_of_returning_old_or_torn_fields() {
        let mailbox = ClockMailbox::default();
        mailbox.version.store(1, Ordering::SeqCst);
        mailbox.fields[0].store(500, Ordering::SeqCst);
        assert!(mailbox.read().is_none());
    }
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
