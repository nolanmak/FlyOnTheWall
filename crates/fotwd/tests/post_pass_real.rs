//! Real-model acceptance for the post-meeting cleanup (#246), outside the
//! ordinary suite: `#[ignore]`d, and it needs the FluidAudio helper, the
//! installed models and public AMI audio.
//!
//! **#241's regression.** Local Whisper dropped 633 of 663 words (base.en)
//! from one ten-minute mic track, AMI ES2004c Headset-2 cropped to 120–720 s
//! (public, CC BY 4.0). This runs that same track through the production
//! cleanup (archived as Opus, Silero, 30 s/2 s windows of Parakeet, overlap
//! merge) and scores it against that headset's own words.
//!
//! ```text
//! FOTW_SPEECHD_PATH=/abs/fotw-speechd \
//! FOTW_POST_PASS_DATA_ROOT=/abs/root     # models/ggml-silero-v5.1.2.bin, models/speechd/…
//! FOTW_241_FIXTURE=/abs/corpus-ami-v2/cropped \
//!   cargo test --release -p fotwd --test post_pass_real -- --ignored --nocapture
//! ```
//!
//! The fixture directory is what `tools/speech-eval/ami_replay.py` builds:
//! `ES2004c-mic.wav` and `ES2004c.reference.json` (`mic_reference` is
//! Headset-2's words in the window).

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use fotw_store::{Db, DbKey, NewMeeting};
use fotwd::post_pass::job::{self, Outcome, Plan};
use fotwd::post_pass::speechd_engine::HelperEngine;
use fotwd::post_pass::turns::Channel;
use fotwd::post_pass::windows::{OpusTrack, PcmSource};

fn env(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")))
}

fn normalize(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// (substitutions + deletions + insertions, deletions) of `hyp` against `reference`.
fn errors(reference: &[String], hyp: &[String]) -> (usize, usize) {
    // (cost, deletions) per cell, one row at a time.
    let mut prev: Vec<(usize, usize)> = (0..=hyp.len()).map(|j| (j, 0)).collect();
    for (i, r) in reference.iter().enumerate() {
        let mut row = vec![(i + 1, i + 1)];
        for (j, h) in hyp.iter().enumerate() {
            let sub = (prev[j].0 + usize::from(r != h), prev[j].1);
            let del = (prev[j + 1].0 + 1, prev[j + 1].1 + 1);
            let ins = (row[j].0 + 1, row[j].1);
            row.push(*[sub, del, ins].iter().min_by_key(|c| c.0).unwrap());
        }
        prev = row;
    }
    prev[hyp.len()]
}

fn read_wav(path: &std::path::Path) -> Vec<i16> {
    let bytes = std::fs::read(path).unwrap();
    let data = bytes
        .windows(4)
        .position(|w| w == b"data")
        .expect("a data chunk");
    bytes[data + 8..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real models and public AMI audio; see the module docs"]
async fn issue_241_the_long_mic_track_whisper_dropped_is_transcribed_whole() {
    let root = env("FOTW_POST_PASS_DATA_ROOT");
    let fixture = env("FOTW_241_FIXTURE");
    let reference: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("ES2004c.reference.json")).unwrap())
            .unwrap();
    let reference = normalize(reference["mic_reference"].as_str().unwrap());
    assert!(reference.len() > 600, "{}", reference.len());

    let dir = tempfile::tempdir().unwrap();
    let opus = dir.path().join("mic.opus");
    let pcm = read_wav(&fixture.join("ES2004c-mic.wav"));
    let mut writer = fotw_pipeline::opus::OpusOggWriter::create(&opus, 16_000).unwrap();
    writer
        .push_f32(
            &pcm.iter()
                .map(|&x| f32::from(x) / 32768.0)
                .collect::<Vec<_>>(),
        )
        .unwrap();
    writer.finish().unwrap();

    let engine = HelperEngine::prepare(&root).await.unwrap();
    let mut db = Db::open_in_memory(&DbKey::from_bytes([0x41; 32])).unwrap();
    let meeting = db.meetings().create(NewMeeting::new("dev", "UTC")).unwrap();
    db.post_pass_queue(
        &meeting,
        &serde_json::to_string(&Plan::current(&engine)).unwrap(),
    )
    .unwrap();
    let db = Mutex::new(db);
    let open = move |channel: Channel| -> Result<Option<Box<dyn PcmSource>>, String> {
        Ok(match channel {
            Channel::Mic => Some(Box::new(OpusTrack::open(&opus)?) as Box<dyn PcmSource>),
            Channel::System => None,
        })
    };
    let Outcome::Done(transcript) = job::run(
        &db,
        &meeting,
        &engine,
        &open,
        &AtomicBool::new(false),
        &|| false,
    )
    .await
    .unwrap() else {
        panic!("the cleanup stopped");
    };
    let mut db = db.into_inner().unwrap();
    let text: String = db
        .meetings()
        .transcript_text(&transcript)
        .unwrap()
        .into_iter()
        .map(|(_, t)| t + " ")
        .collect();
    let hyp = normalize(&text);
    let (errors, deletions) = errors(&reference, &hyp);
    let wer = errors as f64 / reference.len() as f64;
    let deleted = deletions as f64 / reference.len() as f64;
    println!(
        "#241 fixture: {} reference words, {} recognized, WER {:.1}%, deletions {:.1}%",
        reference.len(),
        hyp.len(),
        wer * 100.0,
        deleted * 100.0
    );
    // Whisper base.en lost 95% of these words, large-v3-turbo 68%.
    assert!(
        deleted < 0.15,
        "dropout: {:.1}% of words deleted",
        deleted * 100.0
    );
    assert!(wer < 0.30, "WER {:.1}%", wer * 100.0);
}
