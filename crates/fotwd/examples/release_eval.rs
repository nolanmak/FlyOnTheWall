//! Real-engine eval runner for the release review (#330, §7). Never part of
//! CI: it spends a real engine on every meeting in the corpus, and refuses to
//! run unless `FOTW_RELEASE_EVAL=1`.
//!
//! ```text
//! FOTW_RELEASE_EVAL=1 cargo run -p fotwd --example release_eval -- \
//!     [--engine claude-cli|codex|anthropic] [--binary PATH] \
//!     [--corpus tools/release-eval/corpus] [--jobs 4] [--only ID] [--out FILE]
//! ```
//!
//! `anthropic` reads the key from `ANTHROPIC_API_KEY`. The runner drives each
//! meeting through a [`Preparer`] (cut → summarize → render → review) and
//! scores the result per §7:
//!
//! - a planted passage is **caught** if its quote is absent from the final
//!   payload or a finding covers one of its indices (directly, or through a
//!   bullet citing it); a review held as `model_failed` sends nothing, so it
//!   counts as caught and is reported separately;
//! - a clean meeting is a **false hold** if it gets any finding (or is held);
//! - **retention** is the share of clean segments (clean meetings, and the
//!   unplanted segments of planted meetings) kept in the attachment.
//!
//! Output carries meeting ids, categories and numbers only, never corpus text.
//!
//! # The pipeline
//!
//! [`RealPreparer`] runs the production steps the worker's `EnginePreparer`
//! runs: `release::cut::run`, `release::summary::run`,
//! `release::render::render` and `release::review::run`, over an engine built
//! from the flags instead of the library's settings. Engine ids follow
//! `EnginePreparer` (`claude-cli:default`, `codex-cli:default`,
//! `anthropic-api:<model>`); the CLI's version is printed alongside.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fotwd::release;
use fotwd::release::types::{
    BoxFuture, Category, Cut, Finding, IndexMap, Payload, PrepareError, PrepareInput, Preparer,
    ReleaseRecipient, ReleaseSummary, SourceSegment,
};
use futures_util::StreamExt;
use serde::Deserialize;

#[derive(Deserialize)]
struct Meeting {
    id: String,
    kind: String,
    event_title: String,
    tz: String,
    started_at_ms: i64,
    recipients: Vec<Recipient>,
    segments: Vec<Segment>,
    planted: Vec<Planted>,
}

#[derive(Deserialize)]
struct Recipient {
    address: String,
    display_name: Option<String>,
    is_self: bool,
}

#[derive(Deserialize)]
struct Segment {
    offset_s: u64,
    speaker: String,
    text: String,
}

#[derive(Deserialize)]
struct Planted {
    category: Category,
    indices: Vec<usize>,
    quote: String,
}

impl Meeting {
    fn input(&self) -> PrepareInput {
        PrepareInput {
            meeting_id: self.id.clone(),
            event_title: self.event_title.clone(),
            tz: self.tz.clone(),
            started_at_ms: self.started_at_ms,
            segments: self
                .segments
                .iter()
                .enumerate()
                .map(|(index, s)| SourceSegment {
                    index,
                    offset_ms: s.offset_s * 1_000,
                    speaker: s.speaker.clone(),
                    text: s.text.clone(),
                })
                .collect(),
            corrections: vec![],
            include_summary_spellings: true,
            recipients: self
                .recipients
                .iter()
                .map(|r| ReleaseRecipient {
                    address: r.address.clone(),
                    display_name: r.display_name.clone(),
                    is_self: r.is_self,
                })
                .collect(),
        }
    }
}

/// The production cut, summary, render and review over one engine: the same
/// calls `release::engine_preparer::EnginePreparer` makes, without a library.
struct RealPreparer {
    engine: fotwd::engine::Engine,
    engine_id: String,
}

impl Preparer for RealPreparer {
    fn cut<'a>(&'a self, input: &'a PrepareInput) -> BoxFuture<'a, Result<Cut, PrepareError>> {
        Box::pin(release::cut::run(&self.engine, input))
    }

    fn summarize<'a>(
        &'a self,
        input: &'a PrepareInput,
        retained: &'a [usize],
    ) -> BoxFuture<'a, Result<ReleaseSummary, PrepareError>> {
        Box::pin(release::summary::run(&self.engine, input, retained))
    }

    fn render(
        &self,
        input: &PrepareInput,
        retained: &[usize],
        summary: &ReleaseSummary,
    ) -> Result<Payload, PrepareError> {
        release::render::render(input, retained, summary)
    }

    fn review<'a>(
        &'a self,
        input: &'a PrepareInput,
        retained: &'a [usize],
        payload: &'a Payload,
    ) -> BoxFuture<'a, Result<Vec<Finding>, PrepareError>> {
        Box::pin(release::review::run(&self.engine, input, retained, payload))
    }

    fn engine_id(&self) -> String {
        self.engine_id.clone()
    }
}

struct Outcome {
    retained: Vec<usize>,
    payload: Option<Payload>,
    findings: Result<Vec<Finding>, PrepareError>,
}

async fn prepare(p: &dyn Preparer, input: &PrepareInput) -> Outcome {
    let failed = |e| Outcome {
        retained: vec![],
        payload: None,
        findings: Err(e),
    };
    let cut = match p.cut(input).await {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let retained = cut.retained(input.segments.len());
    let summary = match p.summarize(input, &retained).await {
        Ok(s) => s,
        Err(e) => return failed(e),
    };
    let payload = match p.render(input, &retained, &summary) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    let findings = p.review(input, &retained, &payload).await;
    Outcome {
        retained,
        payload: Some(payload),
        findings,
    }
}

fn covers(f: &Finding, map: &IndexMap, indices: &[usize]) -> bool {
    f.indices.iter().any(|i| indices.contains(i))
        || f.bullet.is_some_and(|(s, o)| {
            map.bullets.iter().any(|(bs, bo, cites)| {
                *bs == s && *bo == o && cites.iter().any(|c| indices.contains(c))
            })
        })
}

fn cat_name(c: Category) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[derive(Default)]
struct Tally {
    planted: usize,
    caught: usize,
    caught_same_category: usize,
    removed_by_cut: usize,
    covered_by_finding: usize,
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn engine(args: &[String]) -> Result<(fotwd::engine::Engine, String), String> {
    let kind = arg(args, "--engine").unwrap_or_else(|| "claude-cli".into());
    let binary = |name: &str| -> Result<PathBuf, String> {
        if let Some(b) = arg(args, "--binary") {
            return Ok(PathBuf::from(b));
        }
        std::env::var_os("PATH")
            .and_then(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join(name))
                    .find(|c| c.is_file())
            })
            .ok_or_else(|| format!("no {name} on PATH; pass --binary"))
    };
    let version = |bin: &Path| {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    match kind.as_str() {
        "claude-cli" => {
            let bin = binary("claude")?;
            let id = format!("claude-cli:default ({})", version(&bin));
            Ok((
                fotwd::engine::Engine::ClaudeCli {
                    binary: bin,
                    settings_version: None,
                },
                id,
            ))
        }
        "codex" => {
            let bin = binary("codex")?;
            let id = format!("codex-cli:default ({})", version(&bin));
            Ok((
                fotwd::engine::Engine::Codex {
                    binary: bin,
                    settings_version: None,
                },
                id,
            ))
        }
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| "ANTHROPIC_API_KEY unset")?;
            let model = fotw_summarize::Preset::Balanced
                .prose_model()
                .unwrap_or_default();
            Ok((
                fotwd::engine::Engine::Anthropic {
                    key: fotw_secrets::SecretString::new(key),
                    settings_version: None,
                },
                format!("anthropic-api:{model}"),
            ))
        }
        other => Err(format!("unknown engine {other}")),
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    if std::env::var("FOTW_RELEASE_EVAL").as_deref() != Ok("1") {
        return Err("release_eval spends a real engine; set FOTW_RELEASE_EVAL=1".into());
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let corpus =
        PathBuf::from(arg(&args, "--corpus").unwrap_or_else(|| "tools/release-eval/corpus".into()));
    let jobs: usize = arg(&args, "--jobs")
        .and_then(|j| j.parse().ok())
        .unwrap_or(4);
    let only = arg(&args, "--only");

    let mut meetings = Vec::new();
    let mut paths: Vec<_> = std::fs::read_dir(&corpus)
        .map_err(|e| format!("corpus: {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let m: Meeting =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        if only.as_ref().is_none_or(|o| *o == m.id) {
            meetings.push(m);
        }
    }

    let (engine, engine_id) = engine(&args)?;
    let preparer = RealPreparer { engine, engine_id };
    eprintln!(
        "engine: {}  meetings: {}  jobs: {jobs}",
        preparer.engine_id(),
        meetings.len()
    );

    let p = &preparer;
    let results: Vec<(usize, Outcome)> = futures_util::stream::iter(meetings.iter().enumerate())
        .map(|(i, m)| async move {
            let started = std::time::Instant::now();
            let out = prepare(p, &m.input()).await;
            let status = match &out.findings {
                Ok(f) => format!("{} findings", f.len()),
                Err(e) => e.code().to_owned(),
            };
            eprintln!(
                "{:<40} {status} ({:.0}s)",
                m.id,
                started.elapsed().as_secs_f64()
            );
            (i, out)
        })
        .buffer_unordered(jobs)
        .collect()
        .await;

    let mut by_cat: BTreeMap<String, Tally> = BTreeMap::new();
    let (mut clean, mut false_holds, mut held) = (0usize, 0usize, 0usize);
    let (mut clean_segments, mut clean_kept) = (0usize, 0usize);
    let mut missed = Vec::new();
    let mut held_planted = Vec::new();
    let mut false_hold_ids = Vec::new();
    let mut extra_findings = 0usize;
    let mut low_retention = Vec::new();
    for (i, out) in &results {
        let m = &meetings[*i];
        let planted_idx: Vec<usize> = m.planted.iter().flat_map(|p| p.indices.clone()).collect();
        let (mut here, mut kept) = (0usize, 0usize);
        for idx in 0..m.segments.len() {
            if !planted_idx.contains(&idx) {
                here += 1;
                kept += usize::from(out.retained.contains(&idx));
            }
        }
        clean_segments += here;
        clean_kept += kept;
        if kept * 10 < here * 9 {
            low_retention.push(format!("{} {kept}/{here}", m.id));
        }
        if out.findings.is_err() {
            held += 1;
        }
        if m.kind == "clean" {
            clean += 1;
            if !matches!(&out.findings, Ok(f) if f.is_empty()) {
                false_holds += 1;
                false_hold_ids.push(m.id.clone());
            }
            continue;
        }
        let empty = IndexMap::default();
        let map = out.payload.as_ref().map_or(&empty, |p| &p.index_map);
        let text = out
            .payload
            .as_ref()
            .map(|p| format!("{}\n{}\n{}", p.subject, p.body, p.attachment))
            .unwrap_or_default();
        if let Ok(findings) = &out.findings {
            extra_findings += findings
                .iter()
                .filter(|f| !covers(f, map, &planted_idx))
                .count();
        }
        for planted in &m.planted {
            let t = by_cat.entry(cat_name(planted.category)).or_default();
            t.planted += 1;
            let (caught, same, absent, found) = match &out.findings {
                Err(_) => {
                    held_planted.push(m.id.clone());
                    (true, false, false, false)
                }
                Ok(findings) => {
                    let absent = !text.contains(&planted.quote);
                    let hit: Vec<&Finding> = findings
                        .iter()
                        .filter(|f| covers(f, map, &planted.indices))
                        .collect();
                    (
                        absent || !hit.is_empty(),
                        hit.iter().any(|f| f.category == planted.category),
                        absent,
                        !hit.is_empty(),
                    )
                }
            };
            t.caught += usize::from(caught);
            t.caught_same_category += usize::from(same);
            t.removed_by_cut += usize::from(absent);
            t.covered_by_finding += usize::from(found);
            if !caught {
                missed.push(format!("{} ({})", m.id, cat_name(planted.category)));
            }
        }
    }

    let pct = |a: usize, b: usize| {
        if b == 0 {
            0.0
        } else {
            100.0 * a as f64 / b as f64
        }
    };
    let (planted, caught): (usize, usize) = by_cat
        .values()
        .fold((0, 0), |(p, c), t| (p + t.planted, c + t.caught));
    println!("engine: {}", preparer.engine_id());
    println!(
        "{:<18} {:>7} {:>7} {:>8} {:>8} {:>8} {:>13}",
        "category", "planted", "caught", "caught%", "cut", "finding", "same-category"
    );
    for (cat, t) in &by_cat {
        println!(
            "{cat:<18} {:>7} {:>7} {:>7.1}% {:>8} {:>8} {:>13}",
            t.planted,
            t.caught,
            pct(t.caught, t.planted),
            t.removed_by_cut,
            t.covered_by_finding,
            t.caught_same_category
        );
    }
    println!(
        "{:<18} {planted:>7} {caught:>7} {:>7.1}%",
        "overall",
        pct(caught, planted)
    );
    println!(
        "false holds: {false_holds}/{clean} clean meetings ({:.1}%)",
        pct(false_holds, clean)
    );
    println!(
        "retention: {clean_kept}/{clean_segments} clean segments ({:.1}%)",
        pct(clean_kept, clean_segments)
    );
    println!(
        "held (model_failed etc.): {held}; findings not covering a planted passage in planted meetings: {extra_findings}"
    );
    println!("missed: {missed:?}");
    println!("false-hold meetings: {false_hold_ids:?}");
    println!("held planted meetings: {held_planted:?}");
    println!("meetings under 90% retention: {low_retention:?}");

    if let Some(out) = arg(&args, "--out") {
        let report = serde_json::json!({
            "engine_id": preparer.engine_id(),
            "planted": planted, "caught": caught,
            "by_category": by_cat.iter().map(|(c, t)| (c.clone(), serde_json::json!({
                "planted": t.planted, "caught": t.caught, "caught_same_category": t.caught_same_category,
                "removed_by_cut": t.removed_by_cut, "covered_by_finding": t.covered_by_finding
            }))).collect::<serde_json::Map<_, _>>(),
            "clean_meetings": clean, "false_holds": false_holds,
            "clean_segments": clean_segments, "clean_kept": clean_kept,
            "held": held, "missed": missed, "low_retention": low_retention, "false_hold_ids": false_hold_ids,
        });
        std::fs::write(
            out,
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
