//! `recordings`, and the inventory the retention sweeper decides from (§9.5).
//!
//! The table has been in migration 0001 from the start with nothing writing to
//! it, because nothing promoted a session into `media/`. This module is the
//! other end of that: the row a promotion writes, the row a sweep retires, and
//! the one query that turns the library into the `(now, meetings, settings)`
//! the sweeper is a pure function of.
//!
//! # The query that matters
//!
//! [`Db::audio_inventory`] answers "does a transcript exist for this meeting"
//! with *segments*, not with `meetings.state` and not with the presence of a
//! `transcripts` row. Both of the easier answers are wrong, and wrong in the
//! direction that deletes a user's only copy of a meeting:
//!
//! * `state = 'ready'` is set when a session finishes **whether or not a
//!   provider was configured**. Recording with transcription switched off is a
//!   supported mode, and every one of those meetings is `ready` with no
//!   transcript at all.
//! * a `transcripts` row is created *before* its segments are appended, so
//!   between those two statements — and permanently, if the append failed —
//!   the row exists and the text does not.
//!
//! Only "there is at least one segment" means the meeting survives in text. It
//! is also the conservative direction: a meeting wrongly considered
//! un-transcribed keeps its audio, which costs disk. The other way costs the
//! meeting.

use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::error::{Result, StoreError};
use crate::ids::{new_id, now_ms};

/// One Opus track, as promotion hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRecording {
    /// `mic` or `system`.
    pub channel: String,
    /// Path relative to the data root (§9.7 invariant 5).
    pub rel_path: String,
    /// Size on disk.
    pub bytes: u64,
    /// Duration of the encoded audio.
    pub duration_ms: u64,
    /// Rate the stream was encoded at.
    pub sample_rate_hz: u32,
}

/// One audio file still on disk for a meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFile {
    /// Path relative to the data root.
    pub rel_path: String,
    /// Recorded size, or `None` for a row that predates it (an import). The
    /// caller stats the file in that case rather than guessing zero, because
    /// a budget computed from zeroes is a budget that never evicts.
    pub bytes: Option<u64>,
}

/// One meeting, as the retention sweeper needs to see it.
///
/// Deliberately a flat row of scalars rather than a `Meeting`: the sweeper
/// lives in `fotw-pipeline` and must not learn about SQLite, and this store
/// must not learn about the sweeper. This struct is the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRow {
    /// The meeting.
    pub meeting_id: String,
    /// Epoch milliseconds UTC. The oldest-first eviction key.
    pub started_at_ms: i64,
    /// `recording` | `transcribing` | `ready` | `failed`.
    pub state: String,
    /// `meetings.retain_audio`.
    pub retain_audio: String,
    /// `meetings.retain_audio_days`.
    pub retain_audio_days: Option<i64>,
    /// When a transcript with actual text first existed, or `None`. See the
    /// module docs for why this is not `state == 'ready'`.
    pub transcript_ready_at_ms: Option<i64>,
    /// Bytes of transcript text. Never reclaimable — accounted for so the UI
    /// can show the split and so a test can prove it went untouched.
    pub transcript_bytes: u64,
    /// The audio still on disk. Empty once it has been swept.
    pub audio: Vec<AudioFile>,
    /// The deadline last computed for this meeting's audio, or `None` if it
    /// has none. Refreshed by each sweep so the UI can say when audio goes.
    pub purge_after_ms: Option<i64>,
}

impl Db {
    /// Record a promoted Opus track, or correct the row if it is already
    /// there.
    ///
    /// Idempotent because promotion is: a crash mid-promotion is finished on
    /// the next run, and `UNIQUE (meeting_id, channel)` would otherwise turn
    /// that recovery into an error.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown meeting, and
    /// [`StoreError::Invalid`] for a path that is absolute or contains `..` —
    /// §9.7 invariant 5, checked at the only place it is cheap to check.
    pub fn upsert_recording(&mut self, meeting_id: &str, rec: &NewRecording) -> Result<String> {
        check_rel_path(&rec.rel_path)?;
        let origin: String = self
            .conn()
            .query_row(
                "SELECT origin_device_id FROM meetings WHERE id = ?1",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            })?;

        let now = now_ms();
        // `encrypted = 0`, deliberately and honestly. §10 specifies media at
        // rest under `age` (STREAM/ChaCha20-Poly1305); that is not built yet,
        // and a column that claims otherwise would be a lie the export and
        // backup paths would then repeat to the user.
        self.conn().execute(
            "INSERT INTO recordings (
                 id, meeting_id, channel, rel_path, codec, container,
                 sample_rate, channels, bitrate_bps, duration_ms, bytes,
                 encrypted, state, created_at, updated_at, lamport,
                 origin_device_id
             ) VALUES (?1, ?2, ?3, ?4, 'opus', 'ogg', ?5, 1, 24000, ?6, ?7,
                       0, 'complete', ?8, ?8, 0, ?9)
             ON CONFLICT (meeting_id, channel) DO UPDATE SET
                 rel_path    = excluded.rel_path,
                 sample_rate = excluded.sample_rate,
                 duration_ms = excluded.duration_ms,
                 bytes       = excluded.bytes,
                 state       = 'complete',
                 deleted_at  = NULL,
                 updated_at  = excluded.updated_at,
                 lamport     = recordings.lamport + 1",
            params![
                new_id(),
                meeting_id,
                rec.channel,
                rec.rel_path,
                i64::from(rec.sample_rate_hz),
                i64::try_from(rec.duration_ms).unwrap_or(i64::MAX),
                i64::try_from(rec.bytes).unwrap_or(i64::MAX),
                now,
                origin,
            ],
        )?;

        self.conn()
            .query_row(
                "SELECT id FROM recordings WHERE meeting_id = ?1 AND channel = ?2",
                params![meeting_id, rec.channel],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// Mark a meeting's audio gone, keeping the rows.
    ///
    /// Returns how many rows changed. The row outliving the bytes is what lets
    /// the UI say "audio was deleted on <date>" instead of pretending the
    /// meeting never had any — and it is how the sweeper's own accounting
    /// tells "never recorded" from "reclaimed".
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures.
    pub fn mark_audio_deleted(&mut self, meeting_id: &str, at_ms: i64) -> Result<usize> {
        let n = self.conn().execute(
            "UPDATE recordings
                SET state = 'deleted', deleted_at = ?2, updated_at = ?3,
                    lamport = lamport + 1
              WHERE meeting_id = ?1 AND state = 'complete'",
            params![meeting_id, at_ms, now_ms()],
        )?;
        Ok(n)
    }

    /// Set (or clear) the per-meeting retention override of §9.3.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown meeting.
    pub fn set_retain_audio(
        &mut self,
        meeting_id: &str,
        kind: &str,
        days: Option<i64>,
    ) -> Result<()> {
        let n = self.conn().execute(
            "UPDATE meetings
                SET retain_audio = ?2, retain_audio_days = ?3,
                    updated_at = ?4, lamport = lamport + 1
              WHERE id = ?1",
            params![meeting_id, kind, days, now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            });
        }
        Ok(())
    }

    /// Write back the deadline the current policy gives this meeting's audio.
    ///
    /// Derived state, refreshed by every sweep rather than computed once at
    /// promotion time — the answer moves when the transcript lands and when
    /// the user changes the policy, so a value written once is wrong by the
    /// second day. `recordings_purge_idx` is the index over it.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures.
    pub fn set_purge_after(&mut self, meeting_id: &str, purge_after_ms: Option<i64>) -> Result<()> {
        self.conn().execute(
            "UPDATE recordings SET purge_after_ms = ?2, updated_at = ?3
              WHERE meeting_id = ?1 AND state = 'complete'",
            params![meeting_id, purge_after_ms, now_ms()],
        )?;
        Ok(())
    }

    /// Every meeting, with its audio, its policy, and whether it has a
    /// transcript.
    ///
    /// One pass over `meetings` and one over `recordings`, joined in memory:
    /// the alternative — a row per (meeting, track) — would make the caller
    /// re-group anyway, and a `GROUP_CONCAT` of paths would need re-parsing
    /// with no separator that cannot appear in a filename.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures.
    pub fn audio_inventory(&self) -> Result<Vec<AudioRow>> {
        let conn = self.conn();

        let mut stmt = conn.prepare(
            "SELECT m.id, m.started_at_ms, m.state, m.retain_audio, m.retain_audio_days,
                    (SELECT MIN(t.created_at) FROM transcripts t
                      WHERE t.meeting_id = m.id
                        AND EXISTS (SELECT 1 FROM segments s WHERE s.transcript_id = t.id)),
                    (SELECT COALESCE(SUM(LENGTH(s.text)), 0) FROM segments s
                      WHERE s.meeting_id = m.id)
               FROM meetings m
              ORDER BY m.started_at_ms, m.id",
        )?;
        let mut rows: Vec<AudioRow> = Vec::new();
        for row in stmt.query_map([], |r| {
            Ok(AudioRow {
                meeting_id: r.get(0)?,
                started_at_ms: r.get(1)?,
                state: r.get(2)?,
                retain_audio: r.get(3)?,
                retain_audio_days: r.get(4)?,
                transcript_ready_at_ms: r.get(5)?,
                transcript_bytes: r.get::<_, i64>(6)?.max(0) as u64,
                audio: Vec::new(),
                purge_after_ms: None,
            })
        })? {
            rows.push(row?);
        }

        let mut stmt = conn.prepare(
            "SELECT meeting_id, rel_path, bytes, purge_after_ms
               FROM recordings WHERE state = 'complete' ORDER BY channel",
        )?;
        let files = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        for f in files {
            let (meeting_id, rel_path, bytes, purge_after_ms) = f?;
            // A path that should never have been stored is not resolved and
            // not counted: §9.7 invariant 5 exists so that nothing downstream
            // has to defend itself, and silently following one here would
            // hand the sweeper a `remove_file` outside the data root.
            if check_rel_path(&rel_path).is_err() {
                continue;
            }
            if let Some(row) = rows.iter_mut().find(|r| r.meeting_id == meeting_id) {
                row.audio.push(AudioFile {
                    rel_path,
                    bytes: bytes.and_then(|b| u64::try_from(b).ok()),
                });
                row.purge_after_ms = row.purge_after_ms.or(purge_after_ms);
            }
        }

        Ok(rows)
    }

    /// Read a `settings` value (§9.3), which holds a JSON scalar or document.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Write a `settings` value, replacing whatever was there.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures.
    pub fn put_setting(&mut self, key: &str, value: &str) -> Result<()> {
        let now = now_ms();
        self.conn().execute(
            "INSERT INTO settings (key, value, updated_at, lamport, origin_device_id)
             VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT (key) DO UPDATE SET
                 value = excluded.value,
                 updated_at = excluded.updated_at,
                 lamport = settings.lamport + 1",
            params![key, value, now, self_device_id(self)],
        )?;
        Ok(())
    }
}

/// The device to stamp on rows this install writes.
///
/// Reads `devices.is_self` when there is one and falls back to a literal
/// rather than failing: a settings write must not be the thing that discovers
/// the device table was never seeded.
fn self_device_id(db: &Db) -> String {
    db.conn()
        .query_row("SELECT id FROM devices WHERE is_self = 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
        .unwrap_or_else(|| "local".to_owned())
}

/// §9.7 invariant 5, at the write.
fn check_rel_path(rel: &str) -> Result<()> {
    use std::path::{Component, Path};
    let path = Path::new(rel);
    let bad = rel.is_empty()
        || path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        });
    if bad {
        return Err(StoreError::InvalidArgument(format!(
            "recordings.rel_path `{rel}` is not a path relative to the data root"
        )));
    }
    Ok(())
}
