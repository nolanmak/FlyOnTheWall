//! The post-meeting cleanup pass (#246).
//!
//! **What it does.** After Stop, in local mode, a background job reads the
//! meeting's two archived tracks again, one 30 s window at a time with 2 s of
//! overlap: Silero VAD finds the speech, Parakeet Unified transcribes it with
//! word times, the windows are joined at their shared edge only, and the
//! system track is diarized so remote speakers get labels. The result is
//! stored as a **new transcript version** beside the live one, and made
//! primary. The live version is never rewritten: "Show live transcript"
//! switches back, and exports carry both with their provenance.
//!
//! **When it runs.** [`CleanupMode::Auto`] (the default) runs it only for a
//! meeting no cloud provider transcribed: with Deepgram the live text is
//! already final quality, and a second pass would double the cost.
//! [`CleanupMode::On`] runs it for every meeting; [`CleanupMode::Off`] never.
//!
//! **Ordering.** Summaries and the GitHub push wait for the cleaned version,
//! for at most [`fotw_store::POST_PASS_WAIT_MS`]; after that they use the
//! live one, and the cleanup still finishes.
//!
//! **Restart.** Jobs are rows in the library and every finished window is a
//! checkpoint ([`job`]), so a daemon that restarts resumes where it was.
//!
//! **Recovery of interrupted recordings is separate.** "Retry locally" and
//! startup recovery finish the *live* transcript of audio that was never
//! transcribed, with the saved per-source plan (docs/DURABLE_SPEECH.md). The
//! cleanup reads only finished, archived meetings and always produces a new
//! version. A recovered meeting is eligible for cleanup afterwards like any
//! other, which is how saved Parakeet audio gets its Parakeet transcript.

pub mod controller;
pub mod engine;
pub mod job;
pub mod merge;
pub mod turns;
pub mod windows;

use std::path::{Path, PathBuf};

use fotw_store::Db;
use serde::{Deserialize, Serialize};

/// Settings row key.
pub const SETTINGS_KEY: &str = "post_pass";

/// Whether a meeting gets the cleanup pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CleanupMode {
    /// Only meetings no cloud provider transcribed.
    #[default]
    Auto,
    /// Every meeting, Deepgram ones included.
    On,
    /// Never.
    Off,
}

/// The stored cleanup choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupSettings {
    /// When to run.
    pub mode: CleanupMode,
}

impl CleanupSettings {
    /// The library's choice; a missing or unreadable row is the default.
    #[must_use]
    pub fn read(db: &Db) -> Self {
        db.get_setting(SETTINGS_KEY)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Store it.
    ///
    /// # Errors
    ///
    /// When the library cannot be written.
    pub fn write(self, db: &mut Db) -> Result<(), String> {
        let json = serde_json::to_string(&self).map_err(|e| e.to_string())?;
        db.put_setting(SETTINGS_KEY, &json)
            .map_err(|e| e.to_string())
    }
}

/// Why a meeting was or was not queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Queued {
    /// A job is waiting.
    Yes,
    /// Cleanup is off.
    Off,
    /// A cloud provider transcribed it, and cleanup is on Auto.
    Cloud,
    /// No archived audio.
    NoAudio,
    /// It already has a job.
    Already,
}

/// Queue `meeting_id` for cleanup if its settings and audio allow it, with
/// the plan this build runs. Called after Stop, once the tracks are archived.
///
/// # Errors
///
/// Library failures.
pub fn queue(db: &mut Db, meeting_id: &str, plan: &job::Plan) -> Result<Queued, String> {
    let mode = CleanupSettings::read(db).mode;
    if mode == CleanupMode::Off {
        return Ok(Queued::Off);
    }
    if mode == CleanupMode::Auto
        && db
            .post_pass_used_cloud(meeting_id)
            .map_err(|e| e.to_string())?
    {
        return Ok(Queued::Cloud);
    }
    if db
        .post_pass_tracks(meeting_id)
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        return Ok(Queued::NoAudio);
    }
    let plan = serde_json::to_string(plan).map_err(|e| e.to_string())?;
    Ok(
        if db
            .post_pass_queue(meeting_id, &plan)
            .map_err(|e| e.to_string())?
        {
            Queued::Yes
        } else {
            Queued::Already
        },
    )
}

/// The archived track of `channel`, if the meeting has one.
///
/// # Errors
///
/// Library failures, or a stored path that leaves the data root.
pub fn track_path(
    db: &Db,
    data_root: &Path,
    meeting_id: &str,
    channel: turns::Channel,
) -> Result<Option<PathBuf>, String> {
    let name = match channel {
        turns::Channel::Mic => "mic",
        turns::Channel::System => "system",
    };
    let tracks = db.post_pass_tracks(meeting_id).map_err(|e| e.to_string())?;
    let Some((_, rel)) = tracks.into_iter().find(|(c, _)| c == name) else {
        return Ok(None);
    };
    let rel = Path::new(&rel);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("a saved track's path leaves the library".into());
    }
    Ok(Some(data_root.join(rel)))
}

#[cfg(test)]
mod tests {
    use fotw_store::{DbKey, NewMeeting, NewRecording};

    use super::engine::fake::FakeEngine;
    use super::*;

    fn meeting(provider: &str) -> (Db, String) {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x31; 32])).unwrap();
        let id = db
            .meetings()
            .create(NewMeeting::new("dev-1", "UTC"))
            .unwrap();
        db.meetings()
            .create_transcript(&id, provider, "m", true)
            .unwrap();
        for channel in ["mic", "system"] {
            db.upsert_recording(
                &id,
                &NewRecording {
                    channel: channel.into(),
                    rel_path: format!("media/2026/10/{id}/{channel}.opus"),
                    bytes: 10,
                    duration_ms: 1_000,
                    sample_rate_hz: 16_000,
                },
            )
            .unwrap();
        }
        (db, id)
    }

    fn plan() -> job::Plan {
        job::Plan::current(&FakeEngine::default())
    }

    #[test]
    fn a_deepgram_meeting_gets_no_cleanup_unless_it_is_switched_on() {
        let (mut db, id) = meeting("deepgram");
        assert_eq!(queue(&mut db, &id, &plan()).unwrap(), Queued::Cloud);
        assert!(db.post_pass_job(&id).unwrap().is_none(), "no job ran");

        CleanupSettings {
            mode: CleanupMode::On,
        }
        .write(&mut db)
        .unwrap();
        assert_eq!(queue(&mut db, &id, &plan()).unwrap(), Queued::Yes);
        assert!(db.post_pass_job(&id).unwrap().is_some());
    }

    #[test]
    fn a_local_meeting_is_cleaned_by_default_and_never_when_off() {
        let (mut db, id) = meeting(fotw_stt::speechd::PROVIDER);
        assert_eq!(CleanupSettings::read(&db).mode, CleanupMode::Auto);
        CleanupSettings {
            mode: CleanupMode::Off,
        }
        .write(&mut db)
        .unwrap();
        assert_eq!(queue(&mut db, &id, &plan()).unwrap(), Queued::Off);
        CleanupSettings {
            mode: CleanupMode::Auto,
        }
        .write(&mut db)
        .unwrap();
        assert_eq!(queue(&mut db, &id, &plan()).unwrap(), Queued::Yes);
        assert_eq!(queue(&mut db, &id, &plan()).unwrap(), Queued::Already);
        assert_eq!(
            track_path(&db, Path::new("/data"), &id, turns::Channel::System).unwrap(),
            Some(PathBuf::from(format!(
                "/data/media/2026/10/{id}/system.opus"
            )))
        );
    }

    #[test]
    fn a_fallback_meeting_that_used_deepgram_for_any_stretch_counts_as_cloud() {
        let (mut db, id) = meeting("mixed");
        let transcript = db.meetings().primary_transcript_id(&id).unwrap().unwrap();
        db.conn()
            .execute(
                "INSERT INTO speech_epochs(id,meeting_id,transcript_id,session_id,source,epoch,from_sample,provider,model,capabilities_json)
                 VALUES('e',?1,?2,'s','system',0,0,'deepgram','nova-3','{}')",
                [id.as_str(), transcript.as_str()],
            )
            .unwrap();
        assert_eq!(queue(&mut db, &id, &plan()).unwrap(), Queued::Cloud);
    }

    #[test]
    fn a_meeting_without_archived_audio_is_left_alone() {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x32; 32])).unwrap();
        let id = db
            .meetings()
            .create(NewMeeting::new("dev-1", "UTC"))
            .unwrap();
        assert_eq!(queue(&mut db, &id, &plan()).unwrap(), Queued::NoAudio);
    }
}
