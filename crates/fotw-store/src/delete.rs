//! "Delete this meeting" — one transaction, then the bytes actually leave the
//! file (docs/REQUIREMENTS.md 9.6).

use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::Db;
use crate::error::{Result, StoreError};
use crate::ids::now_ms;

/// What a delete actually removed.
///
/// Returned rather than logged because the UI has to be able to say what it
/// could *not* reach — §9.6 is explicit that text already sent to an STT or
/// LLM provider, and anything already pushed to Notion/Slack/Obsidian, is
/// outside this operation's reach, and that implying otherwise is a lie the
/// product must not tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutcome {
    /// The meeting that was deleted.
    pub meeting_id: String,
    /// False if there was no such meeting; the tombstone is still written, so
    /// the operation is idempotent.
    pub existed: bool,
    /// Files and directories unlinked under the data root.
    pub removed_media: Vec<PathBuf>,
    /// Relative paths that were recorded in the database but were not found on
    /// disk. Surfaced rather than swallowed: a missing recording is either a
    /// prior partial delete or a bug, and both deserve a log line.
    pub missing_media: Vec<String>,
}

impl Db {
    /// Delete a meeting and everything derived from it.
    ///
    /// # What "delete" has to mean here
    ///
    /// A `DELETE FROM meetings` alone is not deletion. It leaves four things
    /// behind, and this method is the list of them:
    ///
    /// 1. **Child rows**, unless `PRAGMA foreign_keys` is on and every child
    ///    edge cascades. Both are true — see the schema and [`Db::open`].
    /// 2. **The provider's raw response.** `media/<yyyy>/<mm>/<id>/raw-<p>.json.zst`
    ///    contains the *entire* transcript and is the single most commonly
    ///    forgotten artifact. It is unlinked with the rest of the meeting's
    ///    media directory.
    /// 3. **Freed pages.** SQLite marks pages free and reuses them later; the
    ///    old bytes stay in the file until then. `PRAGMA secure_delete` (set
    ///    at open) zeroes them, and `PRAGMA incremental_vacuum` returns them
    ///    to the filesystem.
    /// 4. **The write-ahead log.** Every page image that ever held the
    ///    transcript is still in `db.sqlite3-wal` until a checkpoint. The
    ///    trailing `wal_checkpoint(TRUNCATE)` is what makes the WAL zero-length
    ///    again.
    ///
    /// A tombstone carrying id, kind and timestamp — and nothing else, no
    /// title, no snippet, no path — is written in the same transaction, so a
    /// future sync can tell "deleted" from "never seen".
    ///
    /// # Not yet reachable
    ///
    /// §9.6 also calls for deleting cached exports and cancelling queued
    /// integration runs. Neither subsystem has tables or directories in
    /// migration 0001, so there is nothing here to delete yet; when they land
    /// they belong inside this transaction, not beside it.
    pub fn delete_meeting(&mut self, meeting_id: &str) -> Result<DeleteOutcome> {
        let root = self.path().and_then(Path::parent).map(Path::to_path_buf);
        let outcome = delete_meeting_in(self.conn_mut(), root.as_deref(), meeting_id)?;
        Ok(outcome)
    }
}

/// The engine-level half, split out so it can also be exercised against an
/// unencrypted database (see the tests at the bottom of this file).
pub(crate) fn delete_meeting_in(
    conn: &mut Connection,
    root: Option<&Path>,
    meeting_id: &str,
) -> Result<DeleteOutcome> {
    // Read the artifact paths before the rows go away.
    let rel_paths = media_rel_paths(conn, meeting_id)?;

    let existed;
    {
        let tx = conn.transaction()?;

        let origin: Option<String> = tx
            .query_row(
                "SELECT origin_device_id FROM meetings WHERE id = ?1",
                params![meeting_id],
                |r| r.get(0),
            )
            .optional()?;
        existed = origin.is_some();

        // Everything else hangs off this by ON DELETE CASCADE. Listing the
        // child tables here instead would be a maintenance trap: a table added
        // in a later migration would be silently missed, whereas a missing
        // cascade is caught by tests/delete.rs.
        tx.execute("DELETE FROM meetings WHERE id = ?1", params![meeting_id])?;

        // Identity only. If a title or a path ever appears in this statement,
        // the byte-scan acceptance test below is what will catch it.
        tx.execute(
            "INSERT OR REPLACE INTO tombstones (id, kind, deleted_at, origin_device_id, lamport)
             VALUES (?1, 'meeting', ?2, ?3, 0)",
            params![
                meeting_id,
                now_ms(),
                origin.unwrap_or_else(|| "unknown".to_owned())
            ],
        )?;

        tx.commit()?;
    }

    let (removed_media, missing_media) = unlink_media(root, meeting_id, &rel_paths)?;

    reclaim(conn)?;

    Ok(DeleteOutcome {
        meeting_id: meeting_id.to_owned(),
        existed,
        removed_media,
        missing_media,
    })
}

/// Zero and release the pages the delete freed, then empty the WAL.
///
/// The checkpoint *before* the vacuum matters: `incremental_vacuum` can only
/// return pages that the main database file knows are free, and after a busy
/// meeting most of those frames are still sitting in the WAL. The checkpoint
/// *after* matters more: the vacuum's own writes — including the zeroed pages
/// `secure_delete` produced — land in the WAL first, so without it the
/// transcript would still be sitting in `db.sqlite3-wal` in plain form.
fn reclaim(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    conn.execute_batch("PRAGMA incremental_vacuum;")?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

/// Every data-root-relative path this meeting recorded.
fn media_rel_paths(conn: &Connection, meeting_id: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();

    let mut stmt = conn.prepare("SELECT rel_path FROM recordings WHERE meeting_id = ?1")?;
    for row in stmt.query_map(params![meeting_id], |r| r.get::<_, String>(0))? {
        out.push(row?);
    }

    let mut stmt = conn.prepare(
        "SELECT raw_response_rel_path FROM transcripts
         WHERE meeting_id = ?1 AND raw_response_rel_path IS NOT NULL",
    )?;
    for row in stmt.query_map(params![meeting_id], |r| r.get::<_, String>(0))? {
        out.push(row?);
    }

    Ok(out)
}

/// Unlink the meeting's media directory and any stray recorded files.
fn unlink_media(
    root: Option<&Path>,
    meeting_id: &str,
    rel_paths: &[String],
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let mut removed = Vec::new();
    let mut missing = Vec::new();

    let Some(root) = root else {
        // An in-memory library has no media on disk.
        return Ok((removed, missing));
    };

    // Directories first: `media/<yyyy>/<mm>/<meeting_id>/` is the canonical
    // home, and removing it recursively catches artifacts nothing recorded a
    // row for — cached exports, half-written Opus pages, the provider's raw
    // response if its row was lost to a crash.
    for dir in meeting_media_dirs(root, meeting_id) {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed.push(dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::io("unlinking meeting media", &dir, e)),
        }
    }

    // Then anything recorded outside that directory.
    for rel in rel_paths {
        let Some(abs) = resolve_under_root(root, rel) else {
            // §9.7 invariant 5: absolute paths and `..` are forbidden in the
            // database. Refusing to follow one is the difference between a
            // corrupt row and a `remove_dir_all` outside the data root.
            missing.push(rel.clone());
            continue;
        };
        if removed.iter().any(|d| abs.starts_with(d)) {
            continue;
        }
        match std::fs::remove_file(&abs) {
            Ok(()) => removed.push(abs),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(rel.clone()),
            Err(e) => return Err(StoreError::io("unlinking recording", &abs, e)),
        }
    }

    Ok((removed, missing))
}

/// Find `<root>/media/<yyyy>/<mm>/<meeting_id>` without needing a date
/// library: the layout is exactly two levels deep, so a shallow scan is both
/// cheaper and more honest than recomputing the year and month from
/// `started_at_ms` — which would find nothing if the meeting straddled
/// midnight on New Year's Eve in a timezone we no longer know.
fn meeting_media_dirs(root: &Path, meeting_id: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let media = root.join("media");
    let Ok(years) = std::fs::read_dir(&media) else {
        return hits;
    };
    for year in years.flatten() {
        let Ok(months) = std::fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            let candidate = month.path().join(meeting_id);
            if candidate.is_dir() {
                hits.push(candidate);
            }
        }
    }
    hits
}

/// Join a stored relative path onto the data root, rejecting anything that
/// could escape it.
fn resolve_under_root(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if rel.is_absolute() {
        return None;
    }
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir
        )
    }) {
        return None;
    }
    Some(root.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::new_id;

    const PHRASE: &str = "zarquon-fizzbin-quantum-hedgehog";

    /// Build an *unencrypted* library, seed it, delete the meeting, and scan
    /// the raw file bytes.
    ///
    /// # Why unencrypted
    ///
    /// §9.6's acceptance test is "grep the DB file for a distinctive phrase
    /// from the transcript — zero hits". Run against a SQLCipher database that
    /// assertion is *vacuous*: the phrase is never in the file in plaintext
    /// even before the delete, so the test would pass against an
    /// implementation that deleted nothing at all. The property actually worth
    /// proving is that `secure_delete` + `incremental_vacuum` +
    /// `wal_checkpoint(TRUNCATE)` remove the bytes, and that is only
    /// observable with the cipher out of the way. `tests/delete.rs` runs the
    /// same scenario encrypted, which is what proves the shipped
    /// configuration.
    fn seed_and_delete(dir: &Path) -> (PathBuf, DeleteOutcome) {
        let path = dir.join("db.sqlite3");
        let mut db = Db::open_plaintext(&path).unwrap();

        let meeting_id = new_id();
        let transcript_id = new_id();
        {
            let c = db.conn();
            c.execute(
                "INSERT INTO meetings (id, title, started_at_ms, tz, state, created_at, updated_at, origin_device_id)
                 VALUES (?1, 'Board sync', 0, 'UTC', 'ready', 0, 0, 'dev-1')",
                params![meeting_id],
            )
            .unwrap();
            c.execute(
                "INSERT INTO transcripts (id, meeting_id, provider, model, created_at, updated_at, origin_device_id)
                 VALUES (?1, ?2, 'deepgram', 'nova-3', 0, 0, 'dev-1')",
                params![transcript_id, meeting_id],
            )
            .unwrap();
            // Enough rows that the transcript spans many pages: a single short
            // row could survive inside one page that never gets freed, and the
            // scan would then be measuring luck.
            let mut stmt = c
                .prepare(
                    "INSERT INTO segments (id, transcript_id, meeting_id, idx, start_ms, end_ms, channel, text)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'mic', ?7)",
                )
                .unwrap();
            for i in 0..400i64 {
                stmt.execute(params![
                    new_id(),
                    transcript_id,
                    meeting_id,
                    i,
                    i * 1000,
                    i * 1000 + 900,
                    format!("segment {i}: {PHRASE} and more words to fill the page"),
                ])
                .unwrap();
            }
        }
        db.conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();

        // Sanity: the phrase really is in the file before the delete, so a
        // zero-hit result afterwards means something.
        assert!(
            file_contains(&path, PHRASE.as_bytes()),
            "fixture is broken: the phrase should be in the plaintext file before deletion"
        );

        let outcome = db.delete_meeting(&meeting_id).unwrap();
        (path, outcome)
    }

    fn file_contains(path: &Path, needle: &[u8]) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        bytes.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn deleting_a_meeting_removes_the_transcript_bytes_from_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let (path, outcome) = seed_and_delete(dir.path());
        assert!(outcome.existed);

        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{suffix}", path.display()));
            assert!(
                !file_contains(&p, PHRASE.as_bytes()),
                "transcript text survived deletion in {}",
                p.display()
            );
        }
    }

    #[test]
    fn a_relative_path_resolves_and_an_escaping_one_does_not() {
        let root = Path::new("/data/root");
        assert_eq!(
            resolve_under_root(root, "media/2026/08/abc/mic.opus.age"),
            Some(root.join("media/2026/08/abc/mic.opus.age"))
        );
        assert_eq!(resolve_under_root(root, "../../etc/passwd"), None);
        assert_eq!(resolve_under_root(root, "/etc/passwd"), None);
    }
}
