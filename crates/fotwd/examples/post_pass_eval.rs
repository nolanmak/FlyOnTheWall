//! Real-model acceptance runner for the post-meeting cleanup (#246). Not part
//! of the ordinary test suite: it needs the `fotw-speechd` helper built with
//! FluidAudio (`FOTW_SPEECHD_PATH`), a data root whose `models/` holds the
//! installed Parakeet, diarizer and Silero files, and public fixture audio.
//!
//! ```text
//! FOTW_SPEECHD_PATH=/abs/fotw-speechd cargo run --release -p fotwd --example post_pass_eval -- \
//!     --data-root DIR --mic MIC.wav --system SYSTEM.wav --out DIR [--live] [--no-clean]
//! ```
//!
//! - `--live` runs each track through the live local pipeline a meeting leg
//!   uses (endpointer, Silero gate, one Parakeet batch per utterance), as fast
//!   as it will go, and writes `live.json`.
//! - The cleanup encodes both tracks to Opus exactly as promotion does, then
//!   runs the production job over them (Silero, 30 s/2 s windows of Parakeet
//!   with word timings, overlap merge, diarizer, turns) into an in-memory
//!   library, and writes `clean.json`. Archives already in `--out` (from
//!   `--encode-only`) are reused and the WAVs are then not read, so the
//!   measured memory is the cleanup's.
//! - `--bleed off|system|both` and `--bleed-correlation`, `--bleed-gap-db`,
//!   `--bleed-excess-db`, `--bleed-hang-ms`, `--bleed-context-ms`,
//!   `--bleed-lag-ms` override the cross-track bleed suppression, for tuning
//!   on held-out meetings (`tools/speech-eval/bleed-tuning-set.json`).
//! - `--model-memory N` measures the wired memory each model adds, N rounds,
//!   into `model-memory.json`, and runs nothing else.
//!
//! Both are `[{start_ms, end_ms, text, channel, speaker}]`, the hypothesis
//! format `tools/speech-eval/meeting_score.py` scores. `report.json` holds the
//! cleanup's wall time, real-time factor, window counts and the peak resident
//! memory and physical footprint of this process and the helper, sampled every
//! 250 ms. Every WAV must be mono 16 kHz PCM16. No capture, credentials or
//! network.

#[cfg(target_os = "macos")]
fn main() -> Result<(), String> {
    imp::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("post_pass_eval runs on macOS only");
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use fotw_store::{Db, DbKey, NewMeeting};
    use fotwd::post_pass::job::{self, Plan};
    use fotwd::post_pass::speechd_engine::HelperEngine;
    use fotwd::post_pass::turns::Channel;
    use fotwd::post_pass::windows::{OpusTrack, PcmSource};

    pub fn main() -> Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .cloned()
                .ok_or_else(|| format!("missing {name}"))
        };
        let data_root = PathBuf::from(flag("--data-root")?);
        let out = PathBuf::from(flag("--out")?);
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        // Needs no audio.
        if let Ok(rounds) = flag("--model-memory") {
            let rounds: usize = rounds.parse().map_err(|_| "bad --model-memory")?;
            if rounds == 0 {
                return Err("--model-memory needs at least one round".into());
            }
            let memory = runtime.block_on(model_memory(&data_root, rounds))?;
            write(&out.join("model-memory.json"), &memory)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&memory).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        // With both archives already there and no live pass, the WAVs are
        // not read at all, so the process holds no PCM the cleanup never
        // uses and its memory is the cleanup's.
        let archived = !args.iter().any(|a| a == "--live")
            && ["mic.opus", "system.opus"]
                .iter()
                .all(|name| out.join(name).is_file());
        let (mic, system) = if archived {
            (Vec::new(), Vec::new())
        } else {
            (
                read_wav(Path::new(&flag("--mic")?))?,
                read_wav(Path::new(&flag("--system")?))?,
            )
        };
        let samples = if archived {
            let total = |name: &str| OpusTrack::open(&out.join(name)).map(|t| t.total() as usize);
            total("mic.opus")?.max(total("system.opus")?)
        } else {
            mic.len().max(system.len())
        };
        let mut report = serde_json::json!({
            "audio_seconds": samples as f64 / 16_000.0,
        });
        if args.iter().any(|a| a == "--live") {
            let started = Instant::now();
            let live = runtime.block_on(live(&data_root, &mic, &system))?;
            report["live_wall_seconds"] = started.elapsed().as_secs_f64().into();
            write(&out.join("live.json"), &live)?;
        }
        if !args.iter().any(|a| a == "--no-clean") {
            // Archive both tracks as promotion does, then free the PCM: the
            // job reads only the Opus files, so its memory is measured alone.
            // An archive already there (a previous `--encode-only`) is reused.
            for (pcm, name) in [(&mic, "mic.opus"), (&system, "system.opus")] {
                if !out.join(name).is_file() {
                    encode(pcm, &out.join(name))?;
                }
            }
            drop((mic, system));
            if args.iter().any(|a| a == "--encode-only") {
                return Ok(());
            }
            let clean = runtime.block_on(clean(&data_root, &out, &mut report))?;
            write(&out.join("clean.json"), &clean)?;
        }
        write(&out.join("report.json"), &report)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
        Ok(())
    }

    fn write(path: &Path, value: &serde_json::Value) -> Result<(), String> {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The live local pipeline over each track, unpaced.
    async fn live(
        data_root: &Path,
        mic: &[i16],
        system: &[i16],
    ) -> Result<serde_json::Value, String> {
        use fotw_stt::{LocalStream, LocalStreamConfig, Source, StreamEvent};
        let client = fotwd::speechd::helper(data_root).await?;
        let engine = fotw_stt::SpeechdEngine::prepare(client, fotw_stt::speechd::PARAKEET_UNIFIED)
            .await
            .map_err(|e| e.message)?;
        let vad = fotwd::stt::verify_vad_model(data_root)?;
        let gated: Arc<dyn fotw_stt::RecognizerFactory> = Arc::new(
            fotw_stt::vad::GatedFactory::new(Arc::new(engine), move || {
                fotw_stt::whisper::SileroDetector::load(&vad)
                    .map(|d| Box::new(d) as Box<dyn fotw_stt::SpeechDetector>)
            }),
        );
        let mut rows = Vec::new();
        for (source, pcm, channel) in [
            (Source::Mic, mic, "mic"),
            (Source::System, system, "system"),
        ] {
            let (stream, mut events) =
                LocalStream::open(LocalStreamConfig::new("eval", source), Arc::clone(&gated));
            let collect = async {
                let mut out = Vec::new();
                while let Some(event) = events.recv().await {
                    if let StreamEvent::Final(s) = event {
                        out.push(serde_json::json!({
                            "start_ms": s.start_ms, "end_ms": s.end_ms, "text": s.text,
                            "channel": channel, "speaker": s.speaker,
                        }));
                    }
                }
                out
            };
            let feed = async {
                for piece in pcm.chunks(320) {
                    while stream.queued_samples() > 2 * 16_000 {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    stream.write(piece);
                }
                stream.close().await
            };
            let (closed, segments) = tokio::join!(feed, collect);
            closed.map_err(|e| e.to_string())?;
            rows.extend(segments);
        }
        Ok(rows.into())
    }

    fn encode(pcm: &[i16], path: &Path) -> Result<(), String> {
        let mut w =
            fotw_pipeline::opus::OpusOggWriter::create(path, 16_000).map_err(|e| e.to_string())?;
        // In pieces, as the recording pump writes it.
        for piece in pcm.chunks(16_000) {
            let floats: Vec<f32> = piece.iter().map(|&x| f32::from(x) / 32768.0).collect();
            w.push_f32(&floats).map_err(|e| e.to_string())?;
        }
        w.finish().map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn clean(
        data_root: &Path,
        out: &Path,
        report: &mut serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let mic_opus = out.join("mic.opus");
        let system_opus = out.join("system.opus");

        let mut db =
            Db::open_in_memory(&DbKey::from_bytes([0x46; 32])).map_err(|e| e.to_string())?;
        let meeting = db
            .meetings()
            .create(NewMeeting::new("eval", "UTC"))
            .map_err(|e| e.to_string())?;
        db.meetings()
            .create_transcript(&meeting, "eval", "live", true)
            .map_err(|e| e.to_string())?;

        let prepared = Instant::now();
        let engine = Probe {
            inner: HelperEngine::prepare(data_root)
                .await?
                .with_speaker_threshold({
                    let args: Vec<String> = std::env::args().collect();
                    match args.iter().position(|a| a == "--speaker-threshold") {
                        Some(i) => {
                            Some(args[i + 1].parse().map_err(|_| "bad --speaker-threshold")?)
                        }
                        None => fotwd::post_pass::speechd_engine::SPEAKER_THRESHOLD,
                    }
                })
                .with_bleed(bleed_args()?),
            vad_off: std::env::args().any(|a| a == "--vad-off"),
            spans: Arc::new(Mutex::new(None)),
        };
        let prepare_seconds = prepared.elapsed().as_secs_f64();
        let plan = serde_json::to_string(&Plan::current(&engine)).map_err(|e| e.to_string())?;
        db.post_pass_queue(&meeting, &plan)
            .map_err(|e| e.to_string())?;
        let db = Mutex::new(db);

        let helper_pid = fotwd::speechd::helper(data_root).await?.helper_pid();
        let peaks = Arc::new(Peaks::default());
        let sampling = Arc::new(AtomicBool::new(true));
        let sampler = {
            let (peaks, sampling) = (Arc::clone(&peaks), Arc::clone(&sampling));
            std::thread::spawn(move || {
                while sampling.load(Ordering::SeqCst) {
                    peaks.sample(std::process::id(), helper_pid);
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            })
        };

        let open = move |channel: Channel| -> Result<Option<Box<dyn PcmSource>>, String> {
            let path = match channel {
                Channel::Mic => &mic_opus,
                Channel::System => &system_opus,
            };
            Ok(Some(Box::new(OpusTrack::open(path)?) as Box<dyn PcmSource>))
        };
        let started = Instant::now();
        let outcome = job::run(
            &db,
            &meeting,
            &engine,
            &open,
            &AtomicBool::new(false),
            &|| false,
        )
        .await?;
        let wall = started.elapsed().as_secs_f64();
        sampling.store(false, Ordering::SeqCst);
        let _ = sampler.join();
        let job::Outcome::Done(transcript) = outcome else {
            return Err("the cleanup stopped".into());
        };

        let mut db = db.into_inner().map_err(|e| e.to_string())?;
        let job = db
            .post_pass_job(&meeting)
            .map_err(|e| e.to_string())?
            .ok_or("no job")?;
        let audio = report["audio_seconds"].as_f64().unwrap_or(1.0);
        report["clean_prepare_seconds"] = prepare_seconds.into();
        report["clean_wall_seconds"] = wall.into();
        report["clean_rtf"] = (wall / audio).into();
        report["peak_rss_bytes"] = serde_json::json!({
            "parent": peaks.parent_rss.load(Ordering::SeqCst),
            "helper": peaks.helper_rss.load(Ordering::SeqCst),
            "sum_of_peaks": peaks.parent_rss.load(Ordering::SeqCst) + peaks.helper_rss.load(Ordering::SeqCst),
        });
        report["peak_footprint_bytes"] = serde_json::json!({
            "parent": peaks.parent_footprint.load(Ordering::SeqCst),
            "helper": peaks.helper_footprint.load(Ordering::SeqCst),
        });
        report["clean_detail"] = job.detail.clone().into();
        report["vad_off"] = engine.vad_off.into();
        if let Some(spans) = engine.spans.lock().unwrap().take() {
            let rows: Vec<serde_json::Value> = spans
                .iter()
                .map(|s| serde_json::json!({"start_ms": s.start_ms, "end_ms": s.end_ms, "speaker": s.speaker, "channel": "system", "text": ""}))
                .collect();
            write(&out.join("spans.json"), &rows.into())?;
        }
        report["provenance"] = job
            .provenance_json
            .as_deref()
            .and_then(|p| serde_json::from_str(p).ok())
            .unwrap_or_default();
        let rows: Vec<serde_json::Value> = db
            .meetings()
            .transcript_segments(&transcript)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "start_ms": s.start_ms, "end_ms": s.end_ms, "text": s.text,
                    "channel": s.channel, "speaker": s.speaker,
                })
            })
            .collect();
        Ok(rows.into())
    }

    use fotwd::post_pass::engine::{Diarization, PassEngine};
    use fotwd::post_pass::merge::TimedWord;
    use fotwd::post_pass::turns::SpeakerSpan;
    use futures_util::future::BoxFuture;

    /// The production engine, optionally without speech detection, keeping
    /// the diarizer's raw spans for scoring.
    struct Probe {
        inner: HelperEngine,
        vad_off: bool,
        spans: Arc<Mutex<Option<Vec<SpeakerSpan>>>>,
    }

    impl PassEngine for Probe {
        fn speech<'a>(&'a self, pcm: &'a [i16]) -> BoxFuture<'a, Result<Vec<(u64, u64)>, String>> {
            if self.vad_off {
                let len = pcm.len() as u64;
                return Box::pin(async move { Ok(vec![(0, len)]) });
            }
            self.inner.speech(pcm)
        }
        fn transcribe<'a>(
            &'a self,
            pcm: &'a [i16],
        ) -> BoxFuture<'a, Result<Vec<TimedWord>, String>> {
            self.inner.transcribe(pcm)
        }
        fn diarize(&self) -> BoxFuture<'_, Result<Option<Box<dyn Diarization>>, String>> {
            let spans = Arc::clone(&self.spans);
            Box::pin(async move {
                Ok(self
                    .inner
                    .diarize()
                    .await?
                    .map(|inner| Box::new(Keep { inner, spans }) as Box<dyn Diarization>))
            })
        }
        fn provenance(&self) -> serde_json::Value {
            self.inner.provenance()
        }
        fn bleed(&self) -> Option<fotwd::post_pass::bleed::BleedParams> {
            self.inner.bleed()
        }
        fn release_recognizer(&self) -> BoxFuture<'_, ()> {
            self.inner.release_recognizer()
        }
        fn restore_recognizer(&self) -> BoxFuture<'_, ()> {
            self.inner.restore_recognizer()
        }
        fn release(&self) -> BoxFuture<'_, ()> {
            self.inner.release()
        }
        fn notes(&self) -> Vec<String> {
            self.inner.notes()
        }
    }

    struct Keep {
        inner: Box<dyn Diarization>,
        spans: Arc<Mutex<Option<Vec<SpeakerSpan>>>>,
    }

    impl Diarization for Keep {
        fn push<'a>(&'a mut self, pcm: &'a [i16]) -> BoxFuture<'a, Result<(), String>> {
            self.inner.push(pcm)
        }
        fn finish(self: Box<Self>) -> BoxFuture<'static, Result<Vec<SpeakerSpan>, String>> {
            Box::pin(async move {
                let spans = self.inner.finish().await?;
                *self.spans.lock().unwrap() = Some(spans.clone());
                Ok(spans)
            })
        }
    }

    /// Peak resident size and physical footprint of two processes.
    #[derive(Default)]
    struct Peaks {
        parent_rss: AtomicU64,
        helper_rss: AtomicU64,
        parent_footprint: AtomicU64,
        helper_footprint: AtomicU64,
    }

    impl Peaks {
        fn sample(&self, parent: u32, helper: Option<u32>) {
            if let Some((rss, footprint)) = usage(parent) {
                self.parent_rss.fetch_max(rss, Ordering::SeqCst);
                self.parent_footprint.fetch_max(footprint, Ordering::SeqCst);
            }
            if let Some((rss, footprint)) = helper.and_then(usage) {
                self.helper_rss.fetch_max(rss, Ordering::SeqCst);
                self.helper_footprint.fetch_max(footprint, Ordering::SeqCst);
            }
        }
    }

    fn usage(pid: u32) -> Option<(u64, u64)> {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid rusage_info_v2 the call fills.
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid as libc::c_int,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast(),
            )
        };
        (rc == 0).then_some((info.ri_resident_size, info.ri_phys_footprint))
    }

    /// System-wide wired memory (`vm_stat`), MiB. Core ML keeps model weights
    /// wired outside both processes, so this is how their cost shows.
    fn wired_mib() -> Result<f64, String> {
        let out = std::process::Command::new("/usr/bin/vm_stat")
            .output()
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out.stdout);
        let page: f64 = text
            .split("page size of ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .ok_or("vm_stat: no page size")?;
        let pages: f64 = text
            .lines()
            .find(|l| l.starts_with("Pages wired down"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|s| s.trim().trim_end_matches('.').parse().ok())
            .ok_or("vm_stat: no wired pages")?;
        Ok(pages * page / f64::from(1u32 << 20))
    }

    /// `--model-memory N`: the wired memory each cleanup model adds, and
    /// both at once, measured N times in a fresh helper: load and warm, let
    /// it settle, read, unload, read. Medians, so another app's activity
    /// during one round does not decide it.
    async fn model_memory(data_root: &Path, rounds: usize) -> Result<serde_json::Value, String> {
        use fotwd::post_pass::speechd_engine::DIARIZER_ENGINE;
        use fotwd::speechd::PARAKEET;
        let client = fotwd::speechd::helper(data_root).await?;
        let settle = || tokio::time::sleep(std::time::Duration::from_secs(3));
        let load = |engine: &'static str| {
            let client = client.clone();
            async move {
                client.load(engine).await.map_err(|e| e.to_string())?;
                client.warmup(engine).await.map_err(|e| e.to_string())?;
                Ok::<_, String>(())
            }
        };
        let unload = |engine: &'static str| {
            let client = client.clone();
            async move { client.unload(engine).await.map_err(|e| e.to_string()) }
        };
        let (mut asr, mut diarizer, mut both, mut left) = (vec![], vec![], vec![], vec![]);
        for _ in 0..rounds {
            settle().await;
            let idle = wired_mib()?;
            load(PARAKEET.id).await?;
            settle().await;
            asr.push(wired_mib()? - idle);
            load(DIARIZER_ENGINE).await?;
            settle().await;
            both.push(wired_mib()? - idle);
            unload(PARAKEET.id).await?;
            settle().await;
            diarizer.push(wired_mib()? - idle);
            unload(DIARIZER_ENGINE).await?;
            settle().await;
            left.push(wired_mib()? - idle);
        }
        // Sorted copies, so the per-round arrays stay in round order.
        let median = |xs: &[f64]| {
            let mut sorted = xs.to_vec();
            sorted.sort_by(f64::total_cmp);
            sorted[sorted.len() / 2].round()
        };
        Ok(serde_json::json!({
            "rounds": rounds,
            "wired_mib_median": {
                "parakeet": median(&asr),
                "diarizer": median(&diarizer),
                "both": median(&both),
                "after_unloading_both": median(&left),
            },
            "wired_mib_each_round": {"parakeet": asr, "diarizer": diarizer, "both": both, "after_unloading_both": left},
        }))
    }

    /// `--bleed off|system|both` and `--bleed-<setting> <value>` over the
    /// production settings, for tuning on held-out meetings.
    fn bleed_args() -> Result<Option<fotwd::post_pass::bleed::BleedParams>, String> {
        let args: Vec<String> = std::env::args().collect();
        let value = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        let mut params = fotwd::post_pass::bleed::BleedParams::default();
        match value("--bleed").as_deref() {
            None => {}
            Some("off") => return Ok(None),
            Some("system") => (params.system, params.mic) = (true, false),
            Some("both") => (params.system, params.mic) = (true, true),
            Some(other) => return Err(format!("bad --bleed {other}")),
        }
        let number = |name: &str| -> Result<Option<f32>, String> {
            value(name)
                .map(|v| v.parse::<f32>().map_err(|_| format!("bad {name}")))
                .transpose()
        };
        if let Some(v) = number("--bleed-correlation")? {
            params.min_correlation = v;
        }
        if let Some(v) = number("--bleed-gap-db")? {
            params.min_gap_db = v;
        }
        if let Some(v) = number("--bleed-reference-db")? {
            params.min_reference_db = v;
        }
        if let Some(v) = number("--bleed-hang-ms")? {
            params.hang_ms = v as u32;
        }
        if let Some(v) = number("--bleed-context-ms")? {
            params.context_ms = v as u32;
        }
        if let Some(v) = number("--bleed-lag-ms")? {
            params.max_lag_ms = v as u32;
        }
        if let Some(v) = number("--bleed-excess-db")? {
            params.max_excess_db = v;
        }
        Ok(Some(params))
    }

    fn read_wav(path: &Path) -> Result<Vec<i16>, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let bad = |why: &str| format!("{}: {why}", path.display());
        if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err(bad("not a RIFF/WAVE file"));
        }
        let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let u32_at =
            |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let mut at = 12;
        let mut format_ok = false;
        while at + 8 <= bytes.len() {
            let len = u32_at(at + 4) as usize;
            let body = at + 8;
            let end = body
                .checked_add(len)
                .filter(|&e| e <= bytes.len())
                .ok_or_else(|| bad("truncated chunk"))?;
            match &bytes[at..at + 4] {
                b"fmt " => {
                    format_ok = len >= 16
                        && u16_at(body) == 1
                        && u16_at(body + 2) == 1
                        && u32_at(body + 4) == 16_000
                        && u16_at(body + 14) == 16;
                }
                b"data" => {
                    if !format_ok {
                        return Err(bad("expected mono 16 kHz PCM16"));
                    }
                    return Ok(bytes[body..end]
                        .chunks_exact(2)
                        .map(|b| i16::from_le_bytes([b[0], b[1]]))
                        .collect());
                }
                _ => {}
            }
            at = end + (len & 1);
        }
        Err(bad("no data chunk"))
    }
}
