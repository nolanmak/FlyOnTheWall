//! The IOProc → pump handoff.
//!
//! The producer half of this ring runs on a real-time thread, so these tests
//! assert allocation-freedom rather than merely asserting correctness
//! (docs/REQUIREMENTS.md CAP-04).

use fotw_pipeline::ring::AudioRing;
use fotw_pipeline::ring::GenerationRing;
use fotw_pipeline::rt::{RtAlloc, RtGuard, allocation_violations};

#[global_allocator]
static ALLOC: RtAlloc = RtAlloc;

#[test]
fn timing_faults_survive_superseded_callbacks_without_counting_silence() {
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(4);
    let mut pcm = [0.0; 4];
    for (flag, expected) in [(2, 1), (0, 1), (4, 2), (1, 2)] {
        producer.push_capture_block(&[0.5; 2], 0, 0, flag);
        consumer.pop_into(&mut pcm);
        producer.push_capture_block(&[0.5; 2], 0, 1, 0);
        let observed = consumer.capture_observation().unwrap();
        assert_eq!(observed.flags, 0);
        assert_eq!(observed.timing_faults, expected);
        consumer.pop_into(&mut pcm);
    }
}

#[test]
fn capture_clock_tracks_accepted_positions_and_drops_without_allocation() {
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(4);
    assert!(consumer.capture_observation().is_none());
    let before = allocation_violations();
    {
        let _rt = RtGuard::enter();
        producer.push_capture_block(&[0.25; 4], 0, 0, 0);
        producer.push_capture_block(&[0.75; 2], 2, 100, 4);
    }
    assert_eq!(allocation_violations(), before);
    let rejected = consumer.capture_observation().unwrap();
    assert_eq!((rejected.sequence, rejected.sample_start), (2, 4));
    assert_eq!((rejected.sample_count, rejected.dropped_samples), (0, 2));
    assert_eq!(
        (rejected.device_frames, rejected.host_ns, rejected.flags),
        (2, 100, 4)
    );
    let mut pcm = [0.0; 4];
    assert_eq!(consumer.pop_into(&mut pcm), 4);
    assert_eq!(pcm, [0.25; 4]);
    producer.push_capture_block(&[0.5; 2], 3, 200, 2);
    let resumed = consumer.capture_observation().unwrap();
    assert_eq!((resumed.sample_start, resumed.sample_count), (4, 2));
    assert_eq!((resumed.sequence, resumed.dropped_samples), (3, 2));
    producer.push_capture_block(&[], 99, 999, 0);
    assert_eq!(consumer.capture_observation(), Some(resumed));
}

#[test]
fn capture_clock_isolated_from_late_callbacks_of_old_generation() {
    let (producer, old_consumer) = AudioRing::with_capacity_frames(8);
    let generations = GenerationRing::new(producer);
    let mut old = generations.producer();
    old.push_capture_block(&[1.0; 2], 0, 100, 0);
    let mut replacement = generations.producer();
    let current = generations.take_replacement().unwrap();
    assert!(current.capture_observation().is_none());
    replacement.push_capture_block(&[2.0; 2], 0, 200, 2);
    old.push_capture_block(&[1.0; 2], 1, 300, 0);
    assert_eq!(current.capture_observation().unwrap().host_ns, 200);
    assert_eq!(current.capture_observation().unwrap().sample_start, 0);
    assert_eq!(old_consumer.capture_observation().unwrap().host_ns, 300);
}

#[test]
fn untimed_writes_remain_visible_in_capture_sample_positions() {
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(4);
    producer.push_block(&[1.0; 6]); // Four accepted, two dropped.
    consumer.pop_into(&mut [0.0; 4]);
    producer.push_capture_block(&[2.0; 2], 99, 100, 0);
    let clock = consumer.capture_observation().unwrap();
    assert_eq!(
        (
            clock.sample_start,
            clock.sample_count,
            clock.dropped_samples
        ),
        (4, 2, 2)
    );
}

#[test]
fn concurrent_clock_reader_never_combines_different_callbacks() {
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(8);
    let writer = std::thread::spawn(move || {
        for frame in 0..20_000 {
            producer.push_capture_block(&[0.5; 2], frame, frame * 1000, frame as u32);
        }
    });
    let mut pcm = [0.0; 8];
    loop {
        if let Some(observed) = consumer.capture_observation() {
            assert_eq!(observed.host_ns, observed.device_frames * 1000);
            assert_eq!(u64::from(observed.flags), observed.device_frames);
            assert_eq!(observed.sequence, observed.device_frames + 1);
            assert_eq!(
                observed.sample_start + observed.dropped_samples,
                observed.device_frames * 2 + 2 - observed.sample_count as u64
            );
        }
        consumer.pop_into(&mut pcm);
        if consumer.is_abandoned() {
            break;
        }
    }
    writer.join().unwrap();
    assert_eq!(consumer.capture_observation().unwrap().sequence, 20_000);
}

#[test]
fn queued_initial_audio_does_not_shrink_replacement_capacity() {
    let (mut producer, mut old_consumer) = AudioRing::with_capacity_frames(8);
    assert_eq!(producer.push_complete_block(&[1.0; 8]), 8);
    let generations = GenerationRing::new(producer);
    let initial = generations.producer();
    drop(initial);
    let mut replacement = generations.producer();
    assert_eq!(replacement.push_complete_block(&[2.0; 8]), 8);
    let mut current = generations.take_replacement().unwrap();
    let mut samples = [0.0; 8];
    assert_eq!(old_consumer.pop_into(&mut samples), 8);
    assert_eq!(samples, [1.0; 8]);
    assert_eq!(current.pop_into(&mut samples), 8);
    assert_eq!(samples, [2.0; 8]);
}

#[test]
fn writing_from_a_real_time_section_allocates_nothing() {
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(4096);
    let block = [0.25f32; 480];
    let before = allocation_violations();

    let rejected;
    {
        let _rt = RtGuard::enter();
        for i in 0..8 {
            if i % 2 == 0 {
                producer.push_block(&block);
            } else {
                producer.push_complete_block(&block);
            }
        }
        rejected = producer.push_complete_block(&block);
    }

    assert_eq!(
        allocation_violations(),
        before,
        "the producer half must be allocation-free after construction; this is \
         the whole reason the ring is preallocated"
    );
    assert_eq!(
        rejected, 0,
        "the complete-block overflow path is allocation-free too"
    );

    let mut out = vec![0.0f32; 8 * 480];
    let got = consumer.pop_into(&mut out);
    assert_eq!(got, 8 * 480);
    assert!(out.iter().all(|s| *s == 0.25));
}

#[test]
fn an_overrun_drops_and_counts_rather_than_blocking() {
    // Capacity deliberately smaller than what we push. Blocking here would
    // stall the audio thread, which is the one thing that must never happen:
    // the loss must be observable. Samples dropped before the WAL cannot be
    // recovered from that WAL; precise overrun gap placement is separate work.
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(512);
    let block = [1.0f32; 480];

    assert_eq!(producer.push_block(&block), 480, "first block fits");
    let written = producer.push_block(&block);
    assert!(written < 480, "second block cannot fit entirely");
    assert!(producer.dropped_frames() > 0, "the shortfall was counted");
    // And it is readable from the half that can report it. The producer moves
    // onto the audio thread and is never seen again, so a count only the
    // producer can read is a count nobody reads — which is exactly how ring
    // drops stayed invisible in the daemon (#79).
    assert_eq!(
        consumer.dropped_frames(),
        producer.dropped_frames(),
        "both halves read one counter"
    );

    // The ring still yields exactly what it accepted, uncorrupted.
    let mut out = vec![0.0f32; 1024];
    let got = consumer.pop_into(&mut out);
    assert_eq!(got, 480 + written);
    assert!(out[..got].iter().all(|s| *s == 1.0));
}

#[test]
fn data_survives_a_producer_consumer_thread_split() {
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(8192);
    let total = 100 * 160;

    let writer = std::thread::spawn(move || {
        let mut sent = 0usize;
        let mut value = 0.0f32;
        while sent < total {
            let block: Vec<f32> = (0..160).map(|i| value + i as f32).collect();
            // Wait for room for the WHOLE block before pushing. `push_block`
            // deliberately takes what fits and drops the rest — that is correct
            // for the audio thread, which must never retry — so a producer that
            // wants zero loss has to check capacity first. Treating any
            // non-zero return as "the block went in" silently skips the
            // remainder, which is a partial-write bug that only appears when
            // the consumer falls behind.
            while producer.slots() < block.len() {
                std::thread::yield_now();
            }
            let wrote = producer.push_block(&block);
            assert_eq!(wrote, block.len(), "capacity was checked first");
            sent += wrote;
            value += 160.0;
        }
        producer.dropped_frames()
    });

    let mut received = Vec::with_capacity(total);
    let mut scratch = vec![0.0f32; 4096];
    while received.len() < total {
        let n = consumer.pop_into(&mut scratch);
        if n == 0 {
            std::thread::yield_now();
            continue;
        }
        received.extend_from_slice(&scratch[..n]);
    }

    let dropped = writer.join().unwrap();
    assert_eq!(dropped, 0, "a consumer that keeps up must cause no drops");
    assert_eq!(received.len(), total);
    // Sample order must be exactly preserved: a reordering here would be
    // inaudible in a spot check and catastrophic in a transcript.
    for (i, s) in received.iter().enumerate() {
        assert_eq!(*s, i as f32, "sample {i} out of order");
    }
}

#[test]
fn a_dropped_consumer_is_visible_to_the_producer() {
    let (producer, consumer) = AudioRing::with_capacity_frames(512);
    drop(consumer);
    assert!(
        producer.is_abandoned(),
        "the producer must be able to notice the pump died rather than \
         silently writing into a ring nobody drains"
    );
}

#[test]
fn complete_callbacks_preserve_frames_through_wraparound_without_allocating() {
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(5);
    let before = allocation_violations();
    let mut output = [0.0; 8];
    {
        let _guard = RtGuard::enter();
        assert_eq!(producer.push_complete_block(&[1.0, -1.0, 2.0, -2.0]), 4);
        assert_eq!(producer.push_complete_block(&[3.0, -3.0]), 0);
        assert_eq!(consumer.pop_into(&mut output[..2]), 2);
        assert_eq!(producer.push_complete_block(&[4.0, -4.0]), 2);
        assert_eq!(producer.push_complete_block(&[0.0; 6]), 0);
        assert_eq!(producer.push_complete_block(&[]), 0);
    }
    assert_eq!(allocation_violations(), before);
    assert_eq!(consumer.pop_into(&mut output), 4);
    assert_eq!(&output[..4], &[2.0, -2.0, 4.0, -4.0]);
    assert_eq!(consumer.dropped_frames(), 8);
    assert_eq!(producer.dropped_frames(), 8);
}

#[test]
fn abandoning_initial_attempt_isolates_late_pcm_and_retains_replacement_capacity() {
    // Open can fail before requesting a sink, or start can fail after requesting
    // one. Neither case may leave the next successful producer on the old ring.
    for requested in [false, true] {
        let (producer, mut original) = AudioRing::with_capacity_frames(8);
        let generations = GenerationRing::new(producer);
        let mut failed = requested.then(|| generations.producer());
        let mut empty = generations.discard_initial_attempt();
        assert_eq!(empty.slots(), 0);
        if let Some(producer) = &mut failed {
            assert_eq!(producer.push_complete_block(&[-0.9; 2]), 2);
        }
        let mut recovered = generations.producer();
        assert_eq!(recovered.push_complete_block(&[0.75; 8]), 8);
        let mut current = generations
            .take_replacement()
            .expect("recovery has its own consumer");
        let mut pcm = [0.0; 8];
        assert_eq!(empty.pop_into(&mut pcm), 0);
        assert_eq!(current.pop_into(&mut pcm), 8);
        assert_eq!(pcm, [0.75; 8]);
        assert_eq!(original.pop_into(&mut pcm), if requested { 2 } else { 0 });
    }
}
