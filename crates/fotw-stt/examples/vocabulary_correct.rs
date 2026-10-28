//! Apply a vocabulary profile's local corrections to transcripts, offline.
//!
//! Usage: vocabulary_correct PROFILE.json TRIALS < transcripts.jsonl
//!
//! Each input line is a JSON string. Each output line is
//! `{"text": corrected, "p50_ms": .., "p95_ms": .., "max_ms": ..}` over TRIALS
//! warm applications of the already-compiled matcher to that line.
use fotw_stt::{vocabulary::Profile, vocabulary_matcher::VocabularyMatcher};
use std::io::BufRead;
use std::time::Instant;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let [_, profile, trials] = args.as_slice() else {
        return Err("Usage: vocabulary_correct PROFILE.json TRIALS < transcripts.jsonl".into());
    };
    let trials: usize = trials.parse().map_err(|_| "TRIALS must be a number")?;
    let profile = Profile::parse(&std::fs::read(profile).map_err(|e| e.to_string())?)?;
    let matcher = VocabularyMatcher::new(&profile);
    eprintln!("{} correction targets", matcher.len());
    for line in std::io::stdin().lock().lines() {
        let text: String =
            serde_json::from_str(&line.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let corrected = matcher.apply(&text);
        let mut ms: Vec<f64> = (0..trials.max(1))
            .map(|_| {
                let started = Instant::now();
                std::hint::black_box(matcher.apply(std::hint::black_box(&text)));
                started.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        let pick = |p: f64| ms[((p * ms.len() as f64).ceil() as usize).max(1) - 1];
        println!(
            "{}",
            serde_json::json!({"text": corrected, "p50_ms": pick(0.5), "p95_ms": pick(0.95), "max_ms": ms[ms.len() - 1]})
        );
    }
    Ok(())
}
