//! Serve the real UI over a throwaway library, for looking at it.
//!
//!     cargo run -p fotwd --example ui_preview
//!     cargo run -p fotwd --example ui_preview -- --live-notes
//!
//! Every other way into the library goes through audio capture — a TCC grant,
//! a signed build, and an actual meeting to record — so the web UI was
//! unreachable in CI, on a contributor's machine before they have granted
//! anything, and during review. Reviewing a UI by reading its renderer is how
//! it shipped returning transcripts with no speakers and never showing the
//! user's own notes, with 87 tests green.
//!
//! It writes through the same `persist` path a real session uses and serves
//! through the real `WebServer`, so what appears in the browser is what the
//! product produces, not a fixture shaped to flatter it.
//!
//! # Why this does not touch the keychain
//!
//! The library key would normally come from the OS keychain, and on macOS a
//! keychain item's ACL is bound to the code signature that created it — so
//! every rebuild re-signs the binary and the read stalls on an approval dialog
//! that CI cannot display. `OsKeyStore` now gives up after five seconds rather
//! than hanging, which is right for the product and still useless for a
//! preview. So this uses a fixed, published, throwaway key over a temporary
//! directory. It is not a secret and is not meant to be one: nothing real is
//! ever stored under it.
//!
//! Deliberately an example rather than a `fotwd` subcommand: shipping a "write
//! fake meetings into my library" verb in the product binary is how a user
//! ends up with fabricated meetings they cannot tell from real ones.

#[path = "support/live_notes.rs"]
mod live_notes_fixture;

use std::sync::Arc;

use fotw_store::{Db, DbKey, NewSummary};
use fotw_stt::{Source, TimestampSource, TranscriptSegment};
use fotwd::persist;
use fotwd::session::{LegBuffers, SessionOutcome};

fn seg(idx: u64, speaker: &str, text: &str, start: u64, end: u64) -> TranscriptSegment {
    TranscriptSegment {
        id: format!("seg-{idx}"),
        session_id: "seed".into(),
        source: Source::System,
        speaker: Some(speaker.into()),
        text: text.into(),
        start_ms: start,
        end_ms: end,
        words: Vec::new(),
        confidence: Some(0.93),
        language: Some("en".into()),
        is_final: true,
        revision: 0,
        provider: "deepgram".into(),
        model: "nova-3".into(),
        timestamp_source: TimestampSource::Provider,
    }
}

/// Not a secret. See the module comment.
const PREVIEW_KEY: [u8; 32] = [0x5a; 32];

#[tokio::main]
async fn main() -> Result<(), String> {
    // A temporary directory, never the user's library: this writes fabricated
    // meetings, and there is no undo for that.
    let root = std::env::temp_dir().join(format!("fotw-ui-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let mut db = Db::open(root.join("db.sqlite3"), &DbKey::from_bytes(PREVIEW_KEY))
        .map_err(|e| e.to_string())?;

    let meetings: [(&str, &[(&str, &str)]); 3] = [
        (
            "Weekly infra sync",
            &[
                (
                    "S0",
                    "The p99 on the ingest path went from 240 milliseconds to just over a second on Tuesday.",
                ),
                (
                    "S1",
                    "That lines up with the index rebuild. It finished Tuesday afternoon.",
                ),
                (
                    "S0",
                    "Then let's not treat it as a regression yet. Priya, can you pull the before and after?",
                ),
                ("S1", "I'll have numbers by Friday."),
                (
                    "S0",
                    "Good. If it is the rebuild we do nothing. If it isn't, we roll back the batching change.",
                ),
            ],
        ),
        (
            "Design review — onboarding",
            &[
                (
                    "S0",
                    "The problem is we ask for the microphone before the user has any reason to trust us.",
                ),
                (
                    "S1",
                    "Every competitor does it at install time and every competitor has terrible grant rates.",
                ),
                (
                    "S0",
                    "So we ask at first record, in context, and we explain what we do with it.",
                ),
                ("S1", "Agreed. I'll redo the second screen."),
                // ING-11, with something to click. A participant can say
                // anything, and a copy puts what they said on a system-wide
                // clipboard as `text/html`. This line must paste into Slack,
                // Notion and Docs as literal visible text.
                (
                    "S0",
                    "Someone pasted <b>bold</b> and a <script>alert(1)</script> into the doc, so we need to say what happens to that text.",
                ),
            ],
        ),
        (
            "1:1 — quarterly planning",
            &[
                ("S0", "Where do you want to be by the end of the quarter?"),
                (
                    "S1",
                    "Honestly, I want to own the storage layer end to end.",
                ),
                (
                    "S0",
                    "That's reasonable. Let's write it down and revisit in six weeks.",
                ),
            ],
        ),
    ];

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;

    for (day, (title, lines)) in meetings.into_iter().enumerate() {
        let mut segments = Vec::new();
        let mut t = 0u64;
        for (i, (speaker, text)) in lines.iter().enumerate() {
            // Roughly conversational pacing, so the timeline in the UI is not
            // a row of identical blocks.
            let dur = 2_500 + (text.len() as u64 * 45);
            segments.push(seg(i as u64, speaker, text, t, t + dur));
            t += dur + 400;
        }

        // A real start time, one meeting per day going back. Leaving this at 0
        // let `persist` fall back to "now", so `duration_ms` came out as the
        // millisecond or two the insert itself took and the list showed a
        // 30-second meeting as "0 min" — the seed lying about the product
        // rather than the product being wrong.
        let started_at_ms = now_ms - (day as u64 + 1) * 86_400_000 - t;

        let outcome = SessionOutcome {
            dir: root.clone(),
            started_at_ms,
            capture_ended_at_ms: None,
            system_samples: t * 48,
            mic_samples: 0,
            system_buffers: LegBuffers {
                silent: 0,
                total: (t / 10).max(1),
            },
            mic_buffers: None,
            dropped_samples: 0,
            segments,
            stt_errors: Vec::new(),
        };
        let id = persist::persist_session(&mut db, &outcome, title).map_err(|e| e.to_string())?;

        // `persist_session` stamps the end at insert time, which is right for
        // a live recording — you persist the moment it stops — and nonsense
        // for a backdated one: a meeting started three days ago and inserted
        // now came out as a 72-hour meeting. Restamp it to the length of the
        // transcript we just wrote.
        db.meetings()
            .finish(&id, (started_at_ms + t) as i64)
            .map_err(|e| e.to_string())?;

        db.meetings()
            .upsert_note(&id, "- rollback plan?\n- who owns the follow-up\n", &[])
            .map_err(|e| e.to_string())?;

        let transcript_id = db
            .meetings()
            .primary_transcript_id(&id)
            .map_err(|e| e.to_string())?;
        db.meetings()
            .insert_summary(
                &id,
                NewSummary {
                    origin_device_id: "seed".into(),
                    provider: "anthropic".into(),
                    model: "claude-opus-5".into(),
                    prompt_hash: "seed".into(),
                    body_md: format!(
                        "## {title}\n\nSeeded summary. Regenerate against a real key to \
                         replace this — summaries are versioned, so this row stays as \
                         version 1 rather than being overwritten.\n"
                    ),
                    template_id: None,
                    transcript_id,
                    coverage: Some(0.0),
                    input_tokens: None,
                    output_tokens: None,
                    cost_micros: None,
                },
            )
            .map_err(|e| e.to_string())?;

        println!("  seeded {id}  {title}");
    }

    // An unfinished recording's saved notes, for recovery UI checks without audio.
    db.save_session_note(
        &fotw_store::SessionNoteDocument {
            session_id: format!("preview-recovery-{}", std::process::id()),
            started_at_ms: now_ms.saturating_sub(7_200_000) as i64,
            note: fotw_store::NoteDocument {
                revision: 0,
                body_md: "Recovery fixture: revisit the migration plan.\n\nCafé follow-up.".into(),
                anchors: vec![fotw_store::NoteAnchor::new(
                    0,
                    "Recovery fixture: revisit the migration plan.",
                    250,
                )],
            },
        },
        1_000,
    )
    .map_err(|e| e.to_string())?;

    // The real server, the real ingress policy, the real embedded assets.
    let source = Arc::new(fotw_web::StoreSource::new(db));
    if std::env::args().any(|arg| arg == "--live-notes")
        && std::env::args().any(|arg| arg == "--capture-recovery")
    {
        return Err("Choose one preview mode: --live-notes or --capture-recovery".into());
    }
    let fixture_recorder = if std::env::args().any(|arg| arg == "--live-notes") {
        Some(live_notes_fixture::recorder(&root)?)
    } else {
        None
    };
    let recorder = fixture_recorder
        .as_ref()
        .map(|recorder| recorder.clone() as Arc<dyn fotw_web::recorder::RecorderControl>);
    let recorder = recorder.or_else(|| {
        std::env::args()
            .any(|arg| arg == "--capture-recovery")
            .then(|| {
                Arc::new(RecoveryPreview::new()) as Arc<dyn fotw_web::recorder::RecorderControl>
            })
    });
    let server = fotw_web::WebServer::bind_with_recorder(0, source, recorder)
        .await
        .map_err(|e| format!("bind: {e}"))?;
    let state = server.state().clone();
    if let Some(recorder) = fixture_recorder {
        recorder.attach_hub(state.hub().clone());
    }
    let _flusher = state.hub().spawn_flusher();

    println!("\n  {}", state.launch_url());
    println!("\n  Ctrl-C to stop. The library is thrown away on the next run.");
    server.serve().await.map_err(|e| format!("server: {e}"))
}

/// Synthetic health UI fixture. No microphone, system tap, or provider opens.
struct RecoveryPreview {
    started: u64,
    retry: std::sync::Mutex<Option<std::time::Instant>>,
    stopped: std::sync::atomic::AtomicBool,
}
impl RecoveryPreview {
    fn new() -> Self {
        Self {
            started: fotw_store::now_ms() as u64,
            retry: std::sync::Mutex::new(None),
            stopped: false.into(),
        }
    }
}
impl fotw_web::RecorderControl for RecoveryPreview {
    fn start(&self) -> Result<fotw_web::RecordingStatus, fotw_web::RecorderError> {
        Err(fotw_web::RecorderError::AlreadyRecording)
    }
    fn stop(&self) -> Result<fotw_web::RecordingStatus, fotw_web::RecorderError> {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(self.status())
    }
    fn retry_capture(
        &self,
        id: &str,
        source: fotw_web::recorder::CaptureSource,
    ) -> Result<(), fotw_web::RecorderError> {
        if id != "preview-capture-recovery"
            || source != fotw_web::recorder::CaptureSource::Mic
            || self.stopped.load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(fotw_web::RecorderError::NotRecording);
        }
        let mut retry = self.retry.lock().unwrap();
        if retry.is_some() {
            return Err(fotw_web::RecorderError::Failed(
                "retry already requested".into(),
            ));
        }
        *retry = Some(std::time::Instant::now());
        Ok(())
    }
    fn status(&self) -> fotw_web::RecordingStatus {
        use fotw_web::recorder::{CaptureDelivery, CaptureHealth, SourceCaptureStatus};
        if self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
            return fotw_web::RecordingStatus::idle();
        }
        let retry = self.retry.lock().unwrap();
        let state = match *retry {
            None => CaptureDelivery::Exhausted,
            Some(at) if at.elapsed().as_secs() < 10 => CaptureDelivery::Confirming,
            Some(_) => CaptureDelivery::Healthy,
        };
        fotw_web::RecordingStatus::recording(
            self.started,
            (fotw_store::now_ms() as u64).saturating_sub(self.started),
        )
        .with_session_id(Some("preview-capture-recovery".into()))
        .with_capture_health(Some(CaptureHealth {
            system: SourceCaptureStatus {
                state: CaptureDelivery::Healthy,
                attempts: 0,
                max_attempts: 3,
                retry_in_ms: None,
            },
            mic: SourceCaptureStatus {
                state,
                attempts: match state {
                    CaptureDelivery::Exhausted => 3,
                    CaptureDelivery::Confirming => 1,
                    _ => 0,
                },
                max_attempts: 3,
                retry_in_ms: None,
            },
        }))
    }
}
