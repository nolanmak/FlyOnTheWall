//! The post-meeting pass's fakes (#246): `fake-diarizer` and `fake-words`
//! (docs/SPEECHD_PROTOCOL.md, "Engines"). Run against the Rust fake and, with
//! `FOTW_SPEECHD_BIN`, the Swift helper's `--fake-engines`.

use std::time::Duration;

use fotw_speechd_client::message::{CloseReason, EngineKind, ErrorCode, FinalText, Segment};
use fotw_speechd_client::{
    ClientError, DIARIZATION_MAX_UNACKED_MS, SpeechStream, SpeechdClient, StreamEvent,
};

use super::{config, next, stream};

/// 100 ms of 16 kHz audio.
const BLOCK: usize = 1_600;
/// The diarizer's window: 10 s.
const WINDOW: usize = 160_000;

/// One 100 ms block per entry: every sample is `value`.
fn blocks(values: &[i16]) -> Vec<i16> {
    values
        .iter()
        .flat_map(|&v| std::iter::repeat_n(v, BLOCK))
        .collect()
}

/// One 100 ms block whose loudest sample is `peak`, quieter elsewhere.
fn block_peaking_at(peak: i16) -> Vec<i16> {
    let mut block: Vec<i16> = (0..BLOCK)
        .map(|i| if i % 2 == 0 { 5 } else { -5 })
        .collect();
    block[BLOCK / 2] = -peak;
    block
}

async fn started(engine: &str) -> SpeechdClient {
    let client = SpeechdClient::start(config(&[])).await.unwrap();
    client.load(engine).await.unwrap();
    client.warmup(engine).await.unwrap();
    client
}

/// Every event up to and including the terminal one.
async fn rest(stream: &mut SpeechStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = next(stream).await {
        let terminal = matches!(event, StreamEvent::Closed(_) | StreamEvent::Failed(_));
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

fn speaker(start_ms: u64, end_ms: u64, label: &str) -> Segment {
    Segment {
        start_ms,
        end_ms,
        text: None,
        speaker: Some(label.to_owned()),
    }
}

fn word(start_ms: u64, end_ms: u64, text: &str) -> Segment {
    Segment {
        start_ms,
        end_ms,
        text: Some(text.to_owned()),
        speaker: None,
    }
}

#[tokio::test]
async fn fake_diarizer_acknowledges_each_window_and_finishes_with_one_sorted_final() {
    let client = started("fake-diarizer").await;
    let caps = client.capabilities().unwrap();
    let engine = caps.engine("fake-diarizer").unwrap();
    assert_eq!(engine.kind, EngineKind::Diarization);
    assert_eq!(engine.max_audio_ms, Some(14_400_000));
    assert!(engine.partials);

    // In 100 ms blocks: 2 s silence; 3 s loud (the first voice heard, S1);
    // 1 s of noise under the threshold; 6 s quiet (S2) across the 10 s
    // window edge; 1 s loud right after it; 2 s silence; 5.5 s quiet across
    // the 20 s edge into the tail; and half a block, which is ignored.
    let mut pcm = Vec::new();
    pcm.extend(blocks(&[0; 20]));
    pcm.extend((0..30).flat_map(|_| block_peaking_at(20_000)));
    pcm.extend((0..10).flat_map(|_| block_peaking_at(327)));
    pcm.extend((0..60).flat_map(|_| block_peaking_at(328)));
    pcm.extend((0..10).flat_map(|_| block_peaking_at(16_384)));
    pcm.extend(blocks(&[0; 20]));
    pcm.extend((0..55).flat_map(|_| block_peaking_at(16_383)));
    pcm.extend(std::iter::repeat_n(20_000, BLOCK / 2));
    assert_eq!(pcm.len(), 205 * BLOCK + BLOCK / 2);

    let mut s = client.open_stream(stream("fake-diarizer")).await.unwrap();
    // Odd frame sizes, so frames straddle windows and blocks.
    for chunk in pcm.chunks(7_001) {
        s.send_audio(chunk).await.unwrap();
    }
    assert_eq!(s.await_progress(20_000).await.unwrap(), 20_000);
    s.finish().await.unwrap();
    let events = rest(&mut s).await;
    assert_eq!(
        events,
        vec![
            StreamEvent::Final(FinalText {
                stream: s.id(),
                text: String::new(),
                start_ms: 0,
                end_ms: 20_550,
                segments: vec![
                    speaker(2_000, 5_000, "S1"),
                    speaker(6_000, 12_000, "S2"),
                    speaker(12_000, 13_000, "S1"),
                    speaker(15_000, 20_500, "S2"),
                ],
                model: Some("fake-diarizer@fake".to_owned()),
                utterance: None,
            }),
            StreamEvent::Closed(CloseReason::Finished),
        ],
        "one final, after both windows were acknowledged"
    );
}

#[tokio::test]
async fn fake_diarizer_acknowledges_every_window_in_order() {
    let client = started("fake-diarizer").await;
    let mut s = client.open_stream(stream("fake-diarizer")).await.unwrap();
    // Three and a half windows in one send.
    s.send_audio(&vec![0; WINDOW * 7 / 2]).await.unwrap();
    s.finish().await.unwrap();
    let events = rest(&mut s).await;
    let acks: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Partial {
                text,
                start_ms: 0,
                end_ms,
                ..
            } if text.is_empty() => Some(*end_ms),
            _ => None,
        })
        .collect();
    assert_eq!(acks, vec![10_000, 20_000, 30_000]);
    match &events[3] {
        StreamEvent::Final(f) => {
            assert_eq!((f.text.as_str(), f.end_ms), ("", 35_000));
            assert!(f.segments.is_empty(), "silence has no speakers");
        }
        other => panic!("expected the final, got {other:?}"),
    }
    assert_eq!(events.len(), 5);
}

#[tokio::test]
async fn fake_diarizer_refuses_audio_past_four_hours_with_too_long() {
    let client = started("fake-diarizer").await;
    let mut s = client.open_stream(stream("fake-diarizer")).await.unwrap();
    let silence = vec![0; WINDOW];
    let windows = 4 * 3_600 * 16_000 / WINDOW as u64;
    let mut acked_ms = 0;
    for sent in 1..=windows {
        s.send_audio(&silence).await.unwrap();
        let sent_ms = sent * 10_000;
        if sent_ms > acked_ms + DIARIZATION_MAX_UNACKED_MS {
            acked_ms = tokio::time::timeout(
                Duration::from_secs(10),
                s.await_progress(sent_ms - DIARIZATION_MAX_UNACKED_MS),
            )
            .await
            .expect("an acknowledgement within 10 s")
            .unwrap();
        }
    }
    assert_eq!(
        s.await_progress(14_400_000).await.unwrap(),
        14_400_000,
        "exactly 4 hours is allowed"
    );
    s.send_audio(&[0]).await.unwrap();
    match s.await_progress(u64::MAX).await {
        Err(ClientError::Remote {
            code: ErrorCode::TooLong,
            ..
        }) => {}
        other => panic!("expected too_long, got {other:?}"),
    }
}

/// Batch-finish a `fake-words` stream on `pcm`.
async fn words_of(client: &SpeechdClient, pcm: &[i16]) -> FinalText {
    let mut s = client.open_stream(stream("fake-words")).await.unwrap();
    s.send_audio(pcm).await.unwrap();
    s.finish().await.unwrap();
    let mut events = rest(&mut s).await;
    assert_eq!(
        events.pop(),
        Some(StreamEvent::Closed(CloseReason::Finished))
    );
    match events.as_slice() {
        [StreamEvent::Final(f)] => f.clone(),
        other => panic!("expected one final, got {other:?}"),
    }
}

#[tokio::test]
async fn fake_words_turns_constant_blocks_into_timed_words() {
    let client = started("fake-words").await;
    let caps = client.capabilities().unwrap();
    let engine = caps.engine("fake-words").unwrap();
    assert_eq!(
        (engine.kind, engine.max_audio_ms),
        (EngineKind::Batch, Some(600_000))
    );

    let mut mixed = blocks(&[1_200]);
    mixed[100] = 0;
    let mut pcm = blocks(&[1_200, 1_200, 1_200, 0, 0, 1_200, -700, -700]);
    pcm.extend(mixed);
    pcm.extend(blocks(&[1_200, 1_200]));
    pcm.extend(std::iter::repeat_n(5, 1_000)); // a partial block: ignored
    let f = words_of(&client, &pcm).await;
    assert_eq!(f.text, "w1200 w1200 w-700 w1200");
    assert_eq!(
        f.segments,
        vec![
            word(0, 300, "w1200"),
            word(500, 600, "w1200"),
            word(600, 800, "w-700"),
            word(900, 1_100, "w1200"),
        ]
    );
    assert_eq!((f.start_ms, f.end_ms), (0, 1_162));
    assert_eq!(f.model.as_deref(), Some("fake-words@fake"));

    let silent = words_of(&client, &blocks(&[0, 0])).await;
    assert_eq!((silent.text.as_str(), silent.segments.len()), ("", 0));
}

#[tokio::test]
async fn fake_words_cuts_a_word_at_a_window_edge_and_the_next_overlapping_window_has_it_whole() {
    // The daemon's windows overlap: a word straddling the first window's end
    // is cut there, and the next window, starting inside the overlap, holds
    // it whole, with times relative to that window's first sample.
    let client = started("fake-words").await;
    let meeting = blocks(&[0, 400, 0, 0, 0, 0, 0, 0, 900, 900, 900, 900, 0, 300]);
    let first = words_of(&client, &meeting[..10 * BLOCK]).await;
    let second = words_of(&client, &meeting[7 * BLOCK..]).await;
    assert_eq!(
        first.segments,
        vec![word(100, 200, "w400"), word(800, 1_000, "w900")]
    );
    assert_eq!(
        second.segments,
        vec![word(100, 500, "w900"), word(600, 700, "w300")]
    );
    assert_eq!(second.text, "w900 w300");
}
