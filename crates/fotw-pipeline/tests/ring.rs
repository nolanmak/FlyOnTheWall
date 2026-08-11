//! The IOProc → pump handoff.
//!
//! The producer half of this ring runs on a real-time thread, so these tests
//! assert allocation-freedom rather than merely asserting correctness
//! (docs/REQUIREMENTS.md CAP-04).

use fotw_pipeline::ring::AudioRing;
use fotw_pipeline::rt::{RtAlloc, RtGuard, allocation_violations};

#[global_allocator]
static ALLOC: RtAlloc = RtAlloc;

#[test]
fn writing_from_a_real_time_section_allocates_nothing() {
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(4096);
    let block = [0.25f32; 480];
    let before = allocation_violations();

    {
        let _rt = RtGuard::enter();
        for _ in 0..8 {
            producer.push_block(&block);
        }
    }

    assert_eq!(
        allocation_violations(),
        before,
        "the producer half must be allocation-free after construction; this is \
         the whole reason the ring is preallocated"
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
    // the network is the only backpressure point, and a dropped block is
    // recoverable because the WAL is written independently.
    let (mut producer, mut consumer) = AudioRing::with_capacity_frames(512);
    let block = [1.0f32; 480];

    assert_eq!(producer.push_block(&block), 480, "first block fits");
    let written = producer.push_block(&block);
    assert!(written < 480, "second block cannot fit entirely");
    assert!(producer.dropped_frames() > 0, "the shortfall was counted");

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
            let mut wrote = 0;
            while wrote == 0 {
                wrote = producer.push_block(&block);
                if wrote == 0 {
                    std::thread::yield_now();
                }
            }
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
    let (mut producer, consumer) = AudioRing::with_capacity_frames(512);
    drop(consumer);
    assert!(
        producer.is_abandoned(),
        "the producer must be able to notice the pump died rather than \
         silently writing into a ring nobody drains"
    );
}
