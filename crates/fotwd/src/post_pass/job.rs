//! One meeting's cleanup: every window of both saved tracks through speech
//! detection and Parakeet, the system track through the diarizer, and the
//! result stored as a new transcript version (#246).
//!
//! **Checkpoints.** Each window's words are written to the library as soon as
//! the engine returns them, with the job's progress, in one transaction. A
//! job that stops — the daemon quit, crashed or was killed — resumes at its
//! next start from the first window without a checkpoint: no finished window
//! is transcribed again, and because the merge is a pure function of every
//! window's words, the result is identical to an uninterrupted run. A window
//! that was in flight when the daemon died had no result yet and is
//! transcribed once on resume.
//!
//! Diarization is not checkpointed: speaker identities belong to one
//! diarizer stream, so an interrupted diarization starts the channel over.
//! It is the short phase (a fraction of the transcription time).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use fotw_store::Db;
use fotw_stt::{Source, TranscriptSegment, Word};
use serde::{Deserialize, Serialize};

use super::bleed::{BleedFilter, BleedParams};
use super::engine::PassEngine;
use super::merge::{TimedWord, WindowWords, merge_windows};
use super::turns::{Channel, SpeakerSpan, Turn, build_turns};
use super::windows::{PcmSource, WindowReader, millis, samples};

/// Audio kept either side of detected speech when the rest of a window is
/// silenced, so word onsets and tails reach the recognizer.
pub const SPEECH_PAD_MS: u64 = 300;
/// The provider recorded on the cleaned version.
pub const PROVIDER: &str = "fotw-speechd";
/// Audio handed to the diarizer per call.
const DIARIZE_CHUNK: usize = 16_000 * 10;

/// What a job reads and how. Stored with the job; a resumed job refuses
/// checkpoints made under a different plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Format of this record.
    pub version: u32,
    /// Window length, ms.
    pub window_ms: u64,
    /// Overlap between windows, ms.
    pub overlap_ms: u64,
    /// The engines, as [`PassEngine::provenance`] names them.
    pub engines: serde_json::Value,
    /// Cross-track bleed suppression, when on ([`super::bleed`]).
    #[serde(default)]
    pub bleed: Option<BleedParams>,
}

impl Plan {
    /// The plan this build runs with `engine`.
    #[must_use]
    pub fn current(engine: &dyn PassEngine) -> Self {
        Self::new(engine.provenance(), engine.bleed())
    }

    /// This build's plan for `engines` (as [`PassEngine::provenance`] names
    /// them) with `bleed` suppression.
    #[must_use]
    pub fn new(engines: serde_json::Value, bleed: Option<BleedParams>) -> Self {
        Self {
            version: 2,
            window_ms: super::windows::WINDOW_MS,
            overlap_ms: super::windows::OVERLAP_MS,
            engines,
            bleed,
        }
    }
}

/// Opens a saved track; `Ok(None)` when the meeting has none for a channel.
pub type OpenTrack<'a> =
    dyn Fn(Channel) -> Result<Option<Box<dyn PcmSource>>, String> + Send + Sync + 'a;

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The cleaned version is stored; its transcript id.
    Done(String),
    /// Stopped on request; checkpoints kept for the next start.
    Stopped,
}

/// `channel`'s saved track as the job reads it: with every stretch that is
/// only a copy of the other channel silenced, when the plan says so for this
/// channel and the other channel was saved too.
fn open_track(
    open: &OpenTrack<'_>,
    plan: &Plan,
    channel: Channel,
) -> Result<Option<Box<dyn PcmSource>>, String> {
    let Some(track) = open(channel)? else {
        return Ok(None);
    };
    let Some(params) = plan.bleed.filter(|p| p.applies_to(channel)) else {
        return Ok(Some(track));
    };
    let other = match channel {
        Channel::Mic => Channel::System,
        Channel::System => Channel::Mic,
    };
    Ok(Some(match open(other)? {
        Some(reference) => Box::new(BleedFilter::new(track, reference, params)),
        None => track,
    }))
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Mic => "mic",
        Channel::System => "system",
    }
}

/// Zero everything in `pcm` farther than [`SPEECH_PAD_MS`] from `speech`.
fn silence_outside(pcm: &mut [i16], speech: &[(u64, u64)]) {
    let pad = samples(SPEECH_PAD_MS);
    let mut keep = vec![false; pcm.len()];
    for &(s, e) in speech {
        let from = s.saturating_sub(pad) as usize;
        let to = ((e + pad) as usize).min(pcm.len());
        if from < to {
            keep[from..to].iter_mut().for_each(|k| *k = true);
        }
    }
    for (x, k) in pcm.iter_mut().zip(keep) {
        if !k {
            *x = 0;
        }
    }
}

struct Progress {
    done: u64,
    total: u64,
}

impl Progress {
    fn permille(&self) -> i64 {
        if self.total == 0 {
            return 0;
        }
        // Never 1000 before the version is stored.
        ((self.done * 1000 / self.total) as i64).min(999)
    }
}

fn lock(db: &Mutex<Db>) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// How often a paused job looks again whether it may continue.
pub const PAUSE_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Wait while `pause()` says so, between windows: a recording owns the
/// machine and the speech helper. Returns false if `stop` was set meanwhile.
async fn yield_while(pause: &(dyn Fn() -> bool + Sync), stop: &AtomicBool) -> bool {
    while pause() {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        tokio::time::sleep(PAUSE_POLL).await;
    }
    !stop.load(Ordering::SeqCst)
}

/// Run `meeting_id`'s pending job to completion, or until `stop` is set.
/// Between windows it waits while `pause()` is true (a recording is
/// running); its checkpoints make the wait free.
///
/// # Errors
///
/// A track that cannot be read, an engine failure, or a plan that no longer
/// matches the engines. The caller marks the job failed; the live version
/// stays primary.
///
/// However it ends, the engine's models are released afterwards
/// ([`PassEngine::release`]).
pub async fn run(
    db: &Mutex<Db>,
    meeting_id: &str,
    engine: &dyn PassEngine,
    open: &OpenTrack<'_>,
    stop: &AtomicBool,
    pause: &(dyn Fn() -> bool + Sync),
) -> Result<Outcome, String> {
    let outcome = run_job(db, meeting_id, engine, open, stop, pause).await;
    // A recording running now may be using Parakeet: it is left to it.
    if pause() {
        engine.restore_recognizer().await;
    }
    engine.release().await;
    outcome
}

async fn run_job(
    db: &Mutex<Db>,
    meeting_id: &str,
    engine: &dyn PassEngine,
    open: &OpenTrack<'_>,
    stop: &AtomicBool,
    pause: &(dyn Fn() -> bool + Sync),
) -> Result<Outcome, String> {
    let job = lock(db)
        .post_pass_job(meeting_id)
        .map_err(|e| e.to_string())?
        .ok_or("this meeting has no cleanup job")?;
    let plan: Plan = serde_json::from_str(&job.plan_json).map_err(|e| e.to_string())?;
    if plan != Plan::current(engine) {
        return Err(
            "the speech models changed since this cleanup started; the live transcript is kept"
                .into(),
        );
    }

    let mut tracks = Vec::new();
    for channel in [Channel::Mic, Channel::System] {
        if let Some(track) = open_track(open, &plan, channel)? {
            tracks.push((channel, track));
        }
    }
    let windows_of = |track: &dyn PcmSource| {
        super::windows::plan(
            track.total(),
            samples(plan.window_ms),
            samples(plan.overlap_ms),
        )
        .len() as u64
    };
    let system_windows = tracks
        .iter()
        .filter(|(c, _)| *c == Channel::System)
        .map(|(_, t)| windows_of(t.as_ref()))
        .sum::<u64>();
    // Diarization weighs a fifth of the system channel's transcription.
    let diarize_units = system_windows.div_ceil(5);
    let mut progress = Progress {
        done: 0,
        total: tracks
            .iter()
            .map(|(_, t)| windows_of(t.as_ref()))
            .sum::<u64>()
            + diarize_units,
    };

    let mut heard: Vec<(Channel, Vec<WindowWords>)> = Vec::new();
    for (channel, track) in tracks {
        let name = channel_name(channel);
        let mut saved: std::collections::BTreeMap<usize, Vec<TimedWord>> = lock(db)
            .post_pass_windows(meeting_id, name)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(i, json)| {
                Ok((
                    i as usize,
                    serde_json::from_str(&json).map_err(|e: serde_json::Error| e.to_string())?,
                ))
            })
            .collect::<Result<_, String>>()?;
        let mut reader =
            WindowReader::with_sizes(track, samples(plan.window_ms), samples(plan.overlap_ms));
        let bounds: Vec<(u64, u64)> = reader.plan().to_vec();
        progress.done += saved.len() as u64;
        while let Some(mut window) = reader.next_window(|i| saved.contains_key(&i))? {
            if saved.contains_key(&window.index) {
                continue;
            }
            if !yield_while(pause, stop).await {
                return Ok(Outcome::Stopped);
            }
            let speech = engine.speech(&window.pcm).await?;
            let words = if speech.is_empty() {
                Vec::new()
            } else {
                silence_outside(&mut window.pcm, &speech);
                let offset = millis(window.start);
                engine
                    .transcribe(&window.pcm)
                    .await?
                    .into_iter()
                    .map(|w| TimedWord::new(w.text, w.start_ms + offset, w.end_ms + offset))
                    .collect()
            };
            progress.done += 1;
            let json = serde_json::to_string(&words).map_err(|e| e.to_string())?;
            lock(db)
                .post_pass_checkpoint(
                    meeting_id,
                    name,
                    window.index as i64,
                    &json,
                    progress.permille(),
                )
                .map_err(|e| e.to_string())?;
            saved.insert(window.index, words);
        }
        let windows = bounds
            .iter()
            .enumerate()
            .map(|(i, &(s, e))| WindowWords {
                start_ms: millis(s),
                end_ms: millis(e),
                words: saved.remove(&i).unwrap_or_default(),
            })
            .collect();
        heard.push((channel, windows));
    }

    let mut notes = engine.notes();
    let speakers = if heard.iter().any(|(c, _)| *c == Channel::System) {
        // Parakeet's part is done: release it before the diarizer loads, so
        // the two models are never in memory together. Not while a recording
        // runs (its live transcription may use the same model), and if one
        // started meanwhile, it gets the model back.
        if !yield_while(pause, stop).await {
            return Ok(Outcome::Stopped);
        }
        engine.release_recognizer().await;
        if pause() {
            engine.restore_recognizer().await;
        }
        match diarize(
            db,
            meeting_id,
            engine,
            &|c| open_track(open, &plan, c),
            stop,
            pause,
            &mut progress,
            diarize_units,
        )
        .await?
        {
            Diarized::Spans(spans) => Some(super::turns::join_spans(spans)),
            Diarized::Unavailable(why) => {
                notes.push(why);
                None
            }
            Diarized::Stopped => return Ok(Outcome::Stopped),
        }
    } else {
        None
    };

    let merged = |channel| {
        heard
            .iter()
            .find(|(c, _)| *c == channel)
            .map(|(_, w)| merge_windows(w))
            .unwrap_or_default()
    };
    let turns = build_turns(
        &merged(Channel::Mic),
        &merged(Channel::System),
        speakers.as_deref(),
    );
    let model = if speakers.is_some() {
        "parakeet-unified+diarizer"
    } else {
        "parakeet-unified"
    };
    let mut segments: Vec<TranscriptSegment> = turns.iter().map(|t| to_segment(t, model)).collect();
    crate::session::order_segments(&mut segments);
    let echoes = crate::session::dedupe_cross_leg(&mut segments);
    let rows: Vec<_> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| crate::persist::to_row(i as i64, s))
        .collect();
    let provenance = serde_json::json!({
        "kind": "post-meeting-cleanup",
        "issue": 246,
        "engines": plan.engines,
        "window_ms": plan.window_ms,
        "overlap_ms": plan.overlap_ms,
        "speech_pad_ms": SPEECH_PAD_MS,
        "bleed": plan.bleed,
        "speaker_labels": speakers.is_some(),
        "echo_segments_dropped": echoes,
        "live_transcript_id": job.live_transcript_id,
    });
    let detail = (!notes.is_empty()).then(|| notes.join("; "));
    let id = lock(db)
        .post_pass_complete(
            meeting_id,
            PROVIDER,
            model,
            &rows,
            &provenance.to_string(),
            detail.as_deref(),
        )
        .map_err(|e| e.to_string())?;
    Ok(Outcome::Done(id))
}

enum Diarized {
    Spans(Vec<SpeakerSpan>),
    Unavailable(String),
    Stopped,
}

/// The system channel's speakers. A diarizer that is missing or fails
/// leaves the cleaned version without remote labels rather than failing it
/// (Muesli's `RecordedTranscriptDiarization.apply`).
#[allow(clippy::too_many_arguments)] // the run's own inputs, passed through
async fn diarize(
    db: &Mutex<Db>,
    meeting_id: &str,
    engine: &dyn PassEngine,
    open: &OpenTrack<'_>,
    stop: &AtomicBool,
    pause: &(dyn Fn() -> bool + Sync),
    progress: &mut Progress,
    units: u64,
) -> Result<Diarized, String> {
    let unavailable = |e: String| Diarized::Unavailable(format!("speaker labels unavailable: {e}"));
    // The diarizer loads here, after Parakeet's release; one that will not
    // load or start is treated like one that fails mid-channel.
    let mut session = match engine.diarize().await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Ok(Diarized::Unavailable(
                "speaker labels unavailable: no diarizer installed".into(),
            ));
        }
        Err(e) => return Ok(unavailable(e)),
    };
    let Some(mut track) = open(Channel::System)? else {
        return Ok(Diarized::Spans(Vec::new()));
    };
    let total = track.total().max(1);
    let base = progress.done;
    let mut sent = 0u64;
    let mut pcm = Vec::with_capacity(DIARIZE_CHUNK);
    loop {
        if !yield_while(pause, stop).await {
            return Ok(Diarized::Stopped);
        }
        pcm.clear();
        let n = track.read(&mut pcm, DIARIZE_CHUNK)?;
        if n == 0 {
            break;
        }
        if let Err(e) = session.push(&pcm).await {
            return Ok(unavailable(e));
        }
        sent += n as u64;
        progress.done = base + units * sent / total;
        let _ = lock(db).post_pass_progress(meeting_id, progress.permille());
    }
    match session.finish().await {
        Ok(spans) => Ok(Diarized::Spans(spans)),
        Err(e) => Ok(unavailable(e)),
    }
}

fn to_segment(turn: &Turn, model: &str) -> TranscriptSegment {
    let source = match turn.channel {
        Channel::Mic => Source::Mic,
        Channel::System => Source::System,
    };
    let mut segment = TranscriptSegment::new("post-pass", source, PROVIDER, model);
    segment.speaker.clone_from(&turn.speaker);
    segment.text = turn.text();
    segment.start_ms = turn.start_ms;
    segment.end_ms = turn.end_ms;
    segment.is_final = true;
    segment.language = Some("en".into());
    segment.words = turn
        .words
        .iter()
        .map(|(w, speaker)| Word {
            text: w.text.clone(),
            start_ms: w.start_ms,
            end_ms: w.end_ms,
            confidence: None,
            speaker: speaker.clone(),
        })
        .collect();
    segment
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use fotw_store::{DbKey, NewMeeting, NewSegment, PostPassState};

    use super::super::engine::fake::{FakeEngine, words};
    use super::super::windows::MemoryTrack;
    use super::*;

    const BLOCK: usize = 1_600;

    /// A track of fake words: `(value, start block, blocks)`.
    fn track(total_blocks: usize, words: &[(i16, usize, usize)]) -> Vec<i16> {
        let mut pcm = vec![0i16; total_blocks * BLOCK];
        for &(v, at, len) in words {
            pcm[at * BLOCK..(at + len) * BLOCK]
                .iter_mut()
                .for_each(|x| *x = v);
        }
        pcm
    }

    /// Five minutes per channel: a 300 ms word every 600 ms, and a 400 ms
    /// word straddling every window edge. System words alternate between a
    /// quiet and a loud speaker in 6 s turns.
    fn meeting() -> (Vec<i16>, Vec<i16>) {
        let blocks = 3_000;
        let edges: Vec<usize> =
            super::super::windows::plan((blocks * BLOCK) as u64, 480_000, 32_000)
                .windows(2)
                .flat_map(|p| [p[1].0 as usize / BLOCK, p[0].1 as usize / BLOCK])
                .collect();
        let layout = |base: i16, loud: Option<i16>| {
            let mut out = Vec::new();
            let mut at = 1;
            let mut n: i16 = 0;
            while at + 4 < blocks {
                let v = match loud {
                    Some(l) if (at / 60) % 2 == 1 => l + n,
                    _ => base + n,
                };
                if let Some(&edge) = edges.iter().find(|&&e| e >= at && e < at + 6) {
                    out.push((v, edge - 2, 4));
                    at = edge + 4;
                } else {
                    out.push((v, at, 3));
                    at += 6;
                }
                n += 1;
            }
            out
        };
        let mic = layout(1_000, None);
        let system = layout(3_000, Some(20_000));
        (track(blocks, &mic), track(blocks, &system))
    }

    fn library() -> (Mutex<Db>, String) {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x24; 32])).unwrap();
        let meeting = db
            .meetings()
            .create(NewMeeting::new("dev-1", "UTC"))
            .unwrap();
        let live = db
            .meetings()
            .create_transcript(&meeting, "fotw-speechd", "parakeet-unified", true)
            .unwrap();
        db.meetings()
            .append_segments(&live, &[NewSegment::new(0, 0, 900, "live")])
            .unwrap();
        (Mutex::new(db), meeting)
    }

    fn queue(db: &Mutex<Db>, meeting: &str, engine: &FakeEngine) {
        let plan = serde_json::to_string(&Plan::current(engine)).unwrap();
        assert!(lock(db).post_pass_queue(meeting, &plan).unwrap());
    }

    fn opener(
        mic: Vec<i16>,
        system: Vec<i16>,
    ) -> impl Fn(Channel) -> Result<Option<Box<dyn PcmSource>>, String> + Send + Sync {
        move |c| {
            let pcm = match c {
                Channel::Mic => mic.clone(),
                Channel::System => system.clone(),
            };
            Ok(Some(Box::new(MemoryTrack::new(pcm)) as Box<dyn PcmSource>))
        }
    }

    fn stored(db: &Mutex<Db>, meeting: &str) -> Vec<(String, Option<String>, String)> {
        let db = lock(db);
        let id = db
            .post_pass_job(meeting)
            .unwrap()
            .unwrap()
            .cleaned_transcript_id
            .unwrap();
        let mut db2 = db;
        db2.meetings()
            .transcript_segments(&id)
            .unwrap()
            .into_iter()
            .map(|s| (s.channel, s.speaker, s.text))
            .collect()
    }

    #[tokio::test]
    async fn every_word_of_both_channels_lands_once_with_the_system_speakers_labelled() {
        let (mic, system) = meeting();
        let (db, meeting_id) = library();
        let engine = FakeEngine::default();
        queue(&db, &meeting_id, &engine);
        let outcome = run(
            &db,
            &meeting_id,
            &engine,
            &opener(mic.clone(), system.clone()),
            &AtomicBool::new(false),
            &|| false,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Done(_)));
        let segments = stored(&db, &meeting_id);
        let spoken = |c: &str| -> Vec<String> {
            segments
                .iter()
                .filter(|s| s.0 == c)
                .flat_map(|s| s.2.split(' ').map(str::to_owned).collect::<Vec<_>>())
                .collect()
        };
        let expected = |pcm: &[i16]| words(pcm).into_iter().map(|w| w.text).collect::<Vec<_>>();
        assert_eq!(spoken("mic"), expected(&mic));
        assert_eq!(spoken("system"), expected(&system));
        let speakers: std::collections::BTreeSet<_> = segments
            .iter()
            .filter(|s| s.0 == "system")
            .map(|s| s.1.clone())
            .collect();
        assert_eq!(speakers, [Some("S1".into()), Some("S2".into())].into());
        assert!(
            segments
                .iter()
                .filter(|s| s.0 == "mic")
                .all(|s| s.1.as_deref() == Some("me"))
        );
        let job = lock(&db).post_pass_job(&meeting_id).unwrap().unwrap();
        assert_eq!(job.state, PostPassState::Done);
        assert_eq!(job.progress_permille, 1000);
        assert!(job.provenance_json.unwrap().contains("fake-words@fake"));
    }

    #[tokio::test]
    async fn a_job_killed_halfway_resumes_without_redoing_a_window_and_matches_an_uninterrupted_run()
     {
        let (mic, system) = meeting();
        let open = opener(mic, system);

        let (whole_db, whole_meeting) = library();
        let whole = FakeEngine::default();
        queue(&whole_db, &whole_meeting, &whole);
        run(
            &whole_db,
            &whole_meeting,
            &whole,
            &open,
            &AtomicBool::new(false),
            &|| false,
        )
        .await
        .unwrap();
        let windows = whole.transcribed.load(Ordering::SeqCst);

        let (db, meeting_id) = library();
        // The daemon dies with the window after the halfway mark in flight.
        let first = FakeEngine {
            hang_on: Some(windows / 2 + 1),
            ..FakeEngine::default()
        };
        queue(&db, &meeting_id, &first);
        let killed = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run(
                &db,
                &meeting_id,
                &first,
                &open,
                &AtomicBool::new(false),
                &|| false,
            ),
        )
        .await;
        assert!(killed.is_err(), "the run was cut off");
        let job = lock(&db).post_pass_job(&meeting_id).unwrap().unwrap();
        assert_eq!(job.state, PostPassState::Pending);
        assert!(
            (400..=600).contains(&job.progress_permille),
            "{}",
            job.progress_permille
        );

        let second = FakeEngine::default();
        run(
            &db,
            &meeting_id,
            &second,
            &open,
            &AtomicBool::new(false),
            &|| false,
        )
        .await
        .unwrap();
        assert_eq!(
            first.transcribed.load(Ordering::SeqCst) + second.transcribed.load(Ordering::SeqCst),
            windows,
            "no finished window was transcribed twice"
        );
        assert_eq!(stored(&db, &meeting_id), stored(&whole_db, &whole_meeting));
    }

    #[tokio::test]
    async fn without_a_diarizer_the_version_is_stored_unlabelled_and_says_why() {
        let (mic, system) = meeting();
        let (db, meeting_id) = library();
        let engine = FakeEngine {
            no_diarizer: true,
            ..FakeEngine::default()
        };
        queue(&db, &meeting_id, &engine);
        run(
            &db,
            &meeting_id,
            &engine,
            &opener(mic, system),
            &AtomicBool::new(false),
            &|| false,
        )
        .await
        .unwrap();
        assert!(
            stored(&db, &meeting_id)
                .iter()
                .filter(|s| s.0 == "system")
                .all(|s| s.1.is_none())
        );
        let job = lock(&db).post_pass_job(&meeting_id).unwrap().unwrap();
        assert!(job.detail.unwrap().contains("no diarizer"));
    }

    #[tokio::test]
    async fn a_stop_request_keeps_the_checkpoints_and_the_live_version() {
        let (mic, system) = meeting();
        let (db, meeting_id) = library();
        let engine = FakeEngine::default();
        queue(&db, &meeting_id, &engine);
        let stop = AtomicBool::new(true);
        assert_eq!(
            run(
                &db,
                &meeting_id,
                &engine,
                &opener(mic, system),
                &stop,
                &|| false
            )
            .await
            .unwrap(),
            Outcome::Stopped
        );
        let live = lock(&db)
            .post_pass_job(&meeting_id)
            .unwrap()
            .unwrap()
            .live_transcript_id;
        assert_eq!(
            lock(&db)
                .meetings()
                .primary_transcript_id(&meeting_id)
                .unwrap(),
            live
        );
        let _ = Arc::new(AtomicUsize::new(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_waits_between_windows_while_a_recording_runs() {
        let (mic, system) = meeting();
        let (db, meeting_id) = library();
        let engine = FakeEngine::default();
        queue(&db, &meeting_id, &engine);
        let recording = Arc::new(AtomicBool::new(true));
        let busy = Arc::clone(&recording);
        let pause = move || busy.load(Ordering::SeqCst);
        let open = opener(mic, system);
        let stop = AtomicBool::new(false);
        let run = run(&db, &meeting_id, &engine, &open, &stop, &pause);
        tokio::pin!(run);
        // While the recording runs, no window reaches the engine.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1_500), &mut run)
                .await
                .is_err()
        );
        assert_eq!(engine.started.load(Ordering::SeqCst), 0);
        recording.store(false, Ordering::SeqCst);
        assert!(matches!(run.await.unwrap(), Outcome::Done(_)));
        assert!(engine.transcribed.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn the_recognizer_is_released_before_the_diarizer_starts_and_everything_after() {
        let (mic, system) = meeting();
        let (db, meeting_id) = library();
        let engine = FakeEngine::default();
        queue(&db, &meeting_id, &engine);
        let outcome = run(
            &db,
            &meeting_id,
            &engine,
            &opener(mic, system),
            &AtomicBool::new(false),
            &|| false,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Done(_)));
        assert!(
            !engine.diarized_beside_recognizer.load(Ordering::SeqCst),
            "Parakeet and the diarizer were loaded at the same time"
        );
        assert!(engine.released.load(Ordering::SeqCst), "models left loaded");
        assert_eq!(engine.restored.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_stopped_job_releases_its_models_too() {
        let (mic, system) = meeting();
        let (db, meeting_id) = library();
        let engine = FakeEngine::default();
        queue(&db, &meeting_id, &engine);
        let outcome = run(
            &db,
            &meeting_id,
            &engine,
            &opener(mic, system),
            &AtomicBool::new(true),
            &|| false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::Stopped);
        assert!(engine.released.load(Ordering::SeqCst), "models left loaded");
    }

    #[tokio::test]
    async fn a_job_stopped_while_a_recording_runs_leaves_parakeet_to_it() {
        let (mic, system) = meeting();
        let (db, meeting_id) = library();
        let engine = FakeEngine::default();
        queue(&db, &meeting_id, &engine);
        let outcome = run(
            &db,
            &meeting_id,
            &engine,
            &opener(mic, system),
            &AtomicBool::new(true),
            &|| true,
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::Stopped);
        assert_eq!(
            engine.restored.load(Ordering::SeqCst),
            1,
            "the recording's model was not handed over before the release"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recording_that_starts_while_the_recognizer_is_released_gets_it_back() {
        let (mic, system) = meeting();
        let (db, meeting_id) = library();
        let engine = FakeEngine::default();
        queue(&db, &meeting_id, &engine);
        // A recording starts just as the cleanup releases Parakeet, and ends
        // a moment later.
        let released = Arc::clone(&engine.recognizer_released);
        let seen = AtomicUsize::new(0);
        let pause =
            move || released.load(Ordering::SeqCst) && seen.fetch_add(1, Ordering::SeqCst) == 0;
        let outcome = run(
            &db,
            &meeting_id,
            &engine,
            &opener(mic, system),
            &AtomicBool::new(false),
            &pause,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Done(_)));
        assert_eq!(
            engine.restored.load(Ordering::SeqCst),
            1,
            "the live recording's model was left unloaded"
        );
    }

    #[tokio::test]
    async fn the_diarizer_hears_the_remote_person_but_not_the_mic_speakers_copy() {
        use super::super::bleed::tests::{copy, mix, talker};
        // 60 s: the person at the mic talks for the first 30 s and the
        // system track carries them 26 dB down and 3 ms late; then a remote
        // person talks alone for 30 s.
        let mut mic = talker(1, 30.0, 12_000.0);
        mic.resize(16_000 * 60, 0);
        let mut remote = vec![0i16; 16_000 * 30];
        remote.extend(talker(2, 30.0, 6_000.0));
        let system = mix(&copy(&mic, 0.05, 48), &remote);
        let audible = |pcm: &[i16]| pcm.iter().filter(|&&x| x != 0).count();

        let (db, meeting_id) = library();
        let engine = FakeEngine {
            bleed: Some(BleedParams::default()),
            ..FakeEngine::default()
        };
        queue(&db, &meeting_id, &engine);
        run(
            &db,
            &meeting_id,
            &engine,
            &opener(mic, system.clone()),
            &AtomicBool::new(false),
            &|| false,
        )
        .await
        .unwrap();
        let diarized = engine.diarizer_heard.load(Ordering::SeqCst);
        assert!(
            diarized >= audible(&remote) * 99 / 100,
            "the remote person was cut: {diarized} of {} samples",
            audible(&remote)
        );
        assert!(
            diarized <= audible(&remote) + audible(&system[..16_000 * 30]) / 10,
            "the copy reached the diarizer: {diarized} samples"
        );
    }

    #[test]
    fn silencing_keeps_padded_speech_only() {
        let mut pcm = vec![7i16; 32_000];
        silence_outside(&mut pcm, &[(16_000, 17_000)]);
        let pad = samples(SPEECH_PAD_MS) as usize;
        assert!(pcm[..16_000 - pad].iter().all(|&x| x == 0));
        assert!(pcm[16_000 - pad..17_000 + pad].iter().all(|&x| x == 7));
        assert!(pcm[17_000 + pad..].iter().all(|&x| x == 0));
    }
}
