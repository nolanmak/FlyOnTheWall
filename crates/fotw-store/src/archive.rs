//! The bulk archive and the importer that makes it mean something (EXP-03).
//!
//! An export nobody can import is a screenshot. The importer is the deliverable
//! here, and the round-trip test in `tests/roundtrip.rs` is what makes
//! "lossless" a fact rather than an adjective.
//!
//! # Shape: a directory, not a ZIP
//!
//! Issue #37 allows either. A directory wins on every axis that matters:
//!
//! * it **streams** — one meeting is read, rendered and written, then dropped,
//!   so peak memory is one meeting rather than one library;
//! * it is **resumable** — an interrupted run leaves valid files behind and
//!   [`ArchiveOptions::resume`] skips the ones that are already good, which a
//!   single-stream ZIP cannot offer without rewriting the whole container;
//! * it is **inspectable** — `cat`, `grep`, `git diff` and Obsidian all work on
//!   it directly, and the Markdown half *is* the Obsidian writer target (EXP-04)
//!   rather than a second rendering of the same data;
//! * it costs no compression dependency, and the large payload (Opus audio) is
//!   already compressed.
//!
//! ```text
//! <archive>/
//!   README.txt          <- says, in words, that this is not encrypted
//!   library.json        <- flyonthewall/library@1: manifest + shared tables
//!   meetings/<uuid>.json <- flyonthewall/meeting@1, one per meeting
//!   markdown/<uuid>.md   <- the human/Obsidian rendering
//!   templates/*.md       <- the template files (issue #36), when asked for
//!   media/...            <- audio, opt-in only
//! ```
//!
//! # This directory is not encrypted
//!
//! The library is SQLCipher-encrypted; **this is not**, and cannot be if other
//! tools are to read it. A bulk archive is therefore a plaintext copy of every
//! meeting the user has ever recorded. It is outside the reach of §9.6's
//! byte-level delete, it inherits whatever permissions its parent directory
//! has, and it will be picked up by whatever backs that directory up. The
//! README written into the archive root says exactly this, the manifest carries
//! `"encryption": "none"`, and the CLI makes the user acknowledge it before a
//! bulk export runs.
//!
//! # Conflict policy, and why not `INSERT OR IGNORE`
//!
//! `INSERT OR IGNORE` is the obvious way to make an import idempotent and it is
//! a silent-loss machine here, because the schema is full of *partial unique
//! indexes* — one default template, one primary transcript per meeting, one
//! current summary per meeting, one self device. Importing an archive into a
//! library that already has a default template would silently drop the
//! archive's, and the user would be told the import succeeded.
//!
//! So every insert is a plain `INSERT` and every failure is classified:
//!
//! 1. **A row with the same primary key already exists** — an idempotent
//!    re-import. Counted, not reported, unless the existing row *differs* from
//!    the archive's, in which case the difference is reported because the
//!    archive did not win.
//! 2. **A different row holds an exclusive flag** (`is_default`, `is_current`,
//!    `is_primary`, `is_self`) — the row is retried with that flag cleared and
//!    the demotion is reported. The data arrives; only the "which one is
//!    current" decision is left to the user, which is the only part a machine
//!    cannot answer.
//! 3. **Anything else** — a hard error that rolls the whole import back. A
//!    half-imported library is worse than none, because nothing tells the user
//!    which half is missing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::{ErrorCode, Transaction};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::{Result, StoreError};
use crate::export::{
    ActionItemRow, AppMetaRow, DeviceRow, FolderRow, MeetingDoc, MeetingRow, MeetingTagRow,
    NoteAnchorRow, NoteRow, ParticipantRow, PersonRow, RecordingRow, SegmentRow, SettingRow,
    SummaryRow, TableRow, TagRow, TemplateRow, TombstoneRow, TranscriptRow,
};
use crate::migrations::LATEST_SCHEMA_VERSION;

/// The versioned document kind for a whole library.
pub const LIBRARY_SCHEMA: &str = "flyonthewall/library@1";

/// What the archive root's README says. Written verbatim so the warning cannot
/// drift away from the one the CLI prints.
pub const PLAINTEXT_WARNING: &str = "\
This archive is NOT ENCRYPTED.

Your FlyOnTheWall library is stored in an encrypted database. This directory is
plain text: every transcript, every note and every summary in it can be read by
anyone — or any program — that can read these files. That is deliberate, because
an export another tool cannot open is not an export. It is also the thing to
understand before you copy this directory to a shared drive, a synced folder or
a backup service.

Two consequences worth stating outright:

  * Deleting a meeting inside FlyOnTheWall does not reach into this directory.
  * If you asked for audio, the recordings here are decrypted too.

library.json         the manifest, and the tables shared across meetings
meetings/<id>.json   one meeting each, complete: segments, word timings,
                     speakers, notes with their anchors, every summary version
markdown/<id>.md     the same meeting as a Markdown note (Obsidian-ready)
templates/*.md       your summary templates, exactly as they are on disk
media/...            audio, only if you asked for it

`fotwd import <this directory>` reads it back with the original identifiers.
";

/// How to write an archive.
#[derive(Debug, Clone, Default)]
pub struct ArchiveOptions {
    /// Copy the audio too. **Off by default** — audio is the overwhelming
    /// majority of the bytes (§9.5: ~20 GB/year for a heavy user) and it is
    /// the part a user is most likely not to want lying around in the clear.
    pub include_audio: bool,
    /// The app data root that `*_rel_path` columns are relative to (§9.7
    /// invariant 5). Required for any file copying; without it the archive is
    /// database content only.
    pub data_root: Option<PathBuf>,
    /// The template directory to copy alongside (issue #36). Templates are
    /// files outside the database, so an archive without them is not a
    /// complete backup.
    pub templates_dir: Option<PathBuf>,
    /// Skip meeting files that are already present and readable, so an
    /// interrupted export can be finished rather than restarted.
    pub resume: bool,
}

/// One step of a bulk export, for a progress bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// Meetings finished so far, 1-based.
    pub done: usize,
    /// Meetings in total.
    pub total: usize,
    /// The one just finished.
    pub meeting_id: String,
}

/// What an export did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArchiveReport {
    /// Meetings in the archive.
    pub meetings: usize,
    /// Meeting files actually written this run.
    pub written: usize,
    /// Meeting files left alone because they were already good (`resume`).
    pub skipped: usize,
    /// Audio files copied.
    pub audio_files: usize,
    /// Bytes of audio copied.
    pub audio_bytes: u64,
    /// Relative paths the database recorded and the filesystem did not have.
    /// Surfaced rather than swallowed: a missing recording is either a prior
    /// partial delete or a bug, and an archive silently missing audio the user
    /// asked for is the worst of the three outcomes.
    pub missing_media: Vec<String>,
}

/// A size estimate, so "include audio?" is answerable before it is answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Projection {
    /// Meetings that would be written.
    pub meetings: usize,
    /// Audio files that would be copied.
    pub audio_files: usize,
    /// Bytes of audio that would be copied.
    pub audio_bytes: u64,
}

/// A row the archive could not place exactly as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The table.
    pub table: &'static str,
    /// The row's identity, primary-key columns joined by `/`.
    pub id: String,
    /// What happened, in words a user can act on.
    pub detail: String,
}

/// What an import did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportReport {
    /// Meeting documents read.
    pub meetings: usize,
    /// Rows inserted.
    pub rows_inserted: usize,
    /// Rows already present, byte for byte. The measure of idempotency.
    pub rows_already_present: usize,
    /// Rows that could not land exactly as written. Never empty *and* silent:
    /// the CLI prints every one.
    pub conflicts: Vec<Conflict>,
    /// Template files restored.
    pub templates_restored: usize,
    /// Media files restored.
    pub media_restored: usize,
}

/// `flyonthewall/library@1` — the manifest and every table not owned by a
/// single meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryManifest {
    /// Always [`LIBRARY_SCHEMA`].
    pub schema: String,
    /// `PRAGMA user_version` of the library this came from. An importer whose
    /// schema is older refuses, for the same reason [`Db::open`] refuses a
    /// database from the future.
    pub schema_version: usize,
    /// When the archive was written, epoch ms UTC.
    pub exported_at_ms: i64,
    /// Always `"none"`. A field rather than a comment so a program reading the
    /// archive can act on it, and so it appears in the file a user opens.
    pub encryption: String,
    /// Whether audio was included.
    pub includes_audio: bool,
    /// Meeting ids, each with a file under `meetings/`.
    pub meetings: Vec<String>,
    /// `devices`.
    pub devices: Vec<DeviceRow>,
    /// `app_meta`.
    pub app_meta: Vec<AppMetaRow>,
    /// `settings`.
    pub settings: Vec<SettingRow>,
    /// `folders`.
    pub folders: Vec<FolderRow>,
    /// `templates` (the database rows; the *files* are under `templates/`).
    pub templates: Vec<TemplateRow>,
    /// `people`.
    pub people: Vec<PersonRow>,
    /// `tags`.
    pub tags: Vec<TagRow>,
    /// `tombstones`.
    pub tombstones: Vec<TombstoneRow>,
}

impl Db {
    /// Estimate what a bulk export would cost, without writing anything.
    ///
    /// Reads `recordings.bytes` rather than stat-ing the files, so the answer
    /// is available immediately; falls back to a stat only where the column is
    /// NULL. Rows whose audio is already gone (`state <> 'complete'`) are not
    /// counted, which is the difference between an honest projection and one
    /// that promises 20 GB and delivers 2.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures.
    pub fn projected_archive_size(&self, data_root: &Path) -> Result<Projection> {
        let meetings: i64 = self
            .conn()
            .query_row("SELECT count(*) FROM meetings", [], |r| r.get(0))?;

        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT rel_path, bytes FROM recordings WHERE state = 'complete'")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
        })?;

        let mut audio_files = 0usize;
        let mut audio_bytes = 0u64;
        for row in rows {
            let (rel_path, bytes) = row?;
            let known = bytes.and_then(|b| u64::try_from(b).ok()).or_else(|| {
                std::fs::metadata(data_root.join(&rel_path))
                    .ok()
                    .map(|m| m.len())
            });
            if let Some(n) = known {
                audio_files += 1;
                audio_bytes += n;
            }
        }

        Ok(Projection {
            meetings: usize::try_from(meetings).unwrap_or(0),
            audio_files,
            audio_bytes,
        })
    }

    /// Write the whole library to `dest` as `library@1`.
    ///
    /// Streams: one meeting is loaded, rendered and written before the next is
    /// read, so peak memory is bounded by the largest single meeting rather
    /// than by the library.
    ///
    /// # Errors
    ///
    /// Refuses a destination that already holds something that is not one of
    /// our archives, rather than scattering files into it. Otherwise
    /// propagates filesystem and SQLite failures.
    pub fn export_library(
        &self,
        dest: &Path,
        opts: &ArchiveOptions,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<ArchiveReport> {
        prepare_destination(dest)?;
        let meetings_dir = dest.join("meetings");
        let markdown_dir = dest.join("markdown");
        create_dir(&meetings_dir)?;
        create_dir(&markdown_dir)?;

        let ids: Vec<String> = {
            let conn = self.conn();
            let mut stmt = conn.prepare("SELECT id FROM meetings ORDER BY started_at_ms, id")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        let mut report = ArchiveReport {
            meetings: ids.len(),
            ..ArchiveReport::default()
        };

        for (i, id) in ids.iter().enumerate() {
            let json_path = meetings_dir.join(format!("{id}.json"));
            if opts.resume && is_intact_meeting_file(&json_path, id) {
                report.skipped += 1;
            } else {
                let doc = self.export_meeting(id)?;
                write_atomic(&json_path, doc.to_json_pretty().as_bytes())?;
                write_atomic(
                    &markdown_dir.join(format!("{id}.md")),
                    doc.to_markdown().as_bytes(),
                )?;
                report.written += 1;
            }
            progress(Progress {
                done: i + 1,
                total: ids.len(),
                meeting_id: id.clone(),
            });
        }

        let manifest = LibraryManifest {
            schema: LIBRARY_SCHEMA.to_owned(),
            schema_version: LATEST_SCHEMA_VERSION,
            exported_at_ms: crate::ids::now_ms(),
            encryption: "none".to_owned(),
            includes_audio: opts.include_audio,
            meetings: ids,
            devices: self.fetch_all("ORDER BY id")?,
            app_meta: self.fetch_all("ORDER BY key")?,
            settings: self.fetch_all("ORDER BY key")?,
            folders: self.fetch_all("ORDER BY id")?,
            templates: self.fetch_all("ORDER BY id")?,
            people: self.fetch_all("ORDER BY id")?,
            tags: self.fetch_all("ORDER BY id")?,
            tombstones: self.fetch_all("ORDER BY id")?,
        };
        let text = serde_json::to_string_pretty(&manifest)
            .map_err(|e| StoreError::InvalidArgument(format!("cannot serialize manifest: {e}")))?;
        write_atomic(&dest.join("library.json"), text.as_bytes())?;
        write_atomic(&dest.join("README.txt"), PLAINTEXT_WARNING.as_bytes())?;

        if let Some(src) = &opts.templates_dir {
            copy_markdown_dir(src, &dest.join("templates"))?;
        }
        if let Some(root) = &opts.data_root {
            self.copy_media_out(root, dest, opts.include_audio, &mut report)?;
        }

        Ok(report)
    }

    /// Read a `library@1` archive back, reusing the original identifiers.
    ///
    /// # Errors
    ///
    /// See [`Db::import_library_with`].
    pub fn import_library(&mut self, src: &Path) -> Result<ImportReport> {
        self.import_library_with(src, None, None)
    }

    /// Read a `library@1` archive back, optionally restoring the template
    /// files and the media alongside the database rows.
    ///
    /// One transaction for the whole archive: a partially imported library is
    /// worse than none, because nothing on the screen tells the user which half
    /// arrived.
    ///
    /// # Errors
    ///
    /// * a manifest that is not `library@1`, or is from a newer schema version
    ///   than this build understands — refused, never partially applied;
    /// * a meeting file that is missing or unparseable;
    /// * a constraint failure this module cannot resolve without inventing
    ///   data.
    pub fn import_library_with(
        &mut self,
        src: &Path,
        templates_dir: Option<&Path>,
        data_root: Option<&Path>,
    ) -> Result<ImportReport> {
        let manifest_path = src.join("library.json");
        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|e| StoreError::io("reading the archive manifest", &manifest_path, e))?;
        let manifest: LibraryManifest = serde_json::from_str(&text).map_err(|e| {
            StoreError::InvalidArgument(format!(
                "{} is not a library@1 manifest: {e}",
                manifest_path.display()
            ))
        })?;

        if manifest.schema != LIBRARY_SCHEMA {
            return Err(StoreError::InvalidArgument(format!(
                "expected a `{LIBRARY_SCHEMA}` archive, found `{}`",
                manifest.schema
            )));
        }
        if manifest.schema_version > LATEST_SCHEMA_VERSION {
            return Err(StoreError::InvalidArgument(format!(
                "this archive was written by a newer version of FlyOnTheWall \
                 (schema v{}, this build understands up to v{LATEST_SCHEMA_VERSION}); \
                 upgrade the app to import it",
                manifest.schema_version
            )));
        }

        // Read and parse every meeting document before opening the write
        // transaction, so a corrupt file fails before anything is touched.
        // Bounded by one meeting at a time is the export's problem; an import
        // has to see the whole archive to be atomic anyway.
        let mut docs = Vec::with_capacity(manifest.meetings.len());
        for id in &manifest.meetings {
            let path = src.join("meetings").join(format!("{id}.json"));
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| StoreError::io("reading a meeting document", &path, e))?;
            let doc = MeetingDoc::from_json(&raw)
                .map_err(|e| StoreError::InvalidArgument(format!("{}: {e}", path.display())))?;
            if &doc.meeting.id != id {
                return Err(StoreError::InvalidArgument(format!(
                    "{} holds meeting `{}`, not `{id}`",
                    path.display(),
                    doc.meeting.id
                )));
            }
            docs.push(doc);
        }

        let mut report = ImportReport {
            meetings: docs.len(),
            ..ImportReport::default()
        };

        let tx = self.conn_mut().transaction()?;
        // Foreign keys are checked at COMMIT rather than per statement, which
        // is what lets rows arrive in file order. `folders.parent_id` is
        // self-referential and `summaries.transcript_id` points sideways, so
        // no single insertion order is safe without this.
        tx.pragma_update(None, "defer_foreign_keys", true)?;

        insert_rows(&tx, &manifest.devices, &mut report)?;
        insert_rows(&tx, &manifest.app_meta, &mut report)?;
        insert_rows(&tx, &manifest.settings, &mut report)?;
        insert_rows(&tx, &manifest.folders, &mut report)?;
        insert_rows(&tx, &manifest.templates, &mut report)?;
        insert_rows(&tx, &manifest.people, &mut report)?;
        insert_rows(&tx, &manifest.tags, &mut report)?;
        insert_rows(&tx, &manifest.tombstones, &mut report)?;

        for doc in &docs {
            insert_rows(&tx, std::slice::from_ref(&doc.meeting), &mut report)?;
            insert_rows(&tx, &doc.participants, &mut report)?;
            insert_rows(&tx, &doc.meeting_tags, &mut report)?;
            insert_rows(&tx, &doc.transcripts, &mut report)?;
            insert_rows(&tx, &doc.segments, &mut report)?;
            insert_rows(&tx, &doc.notes, &mut report)?;
            insert_rows(&tx, &doc.note_anchors, &mut report)?;
            insert_rows(&tx, &doc.summaries, &mut report)?;
            insert_rows(&tx, &doc.action_items, &mut report)?;
            insert_rows(&tx, &doc.recordings, &mut report)?;
        }

        tx.commit()?;

        if let Some(dir) = templates_dir {
            report.templates_restored = copy_markdown_dir(&src.join("templates"), dir)?;
        }
        if let Some(root) = data_root {
            report.media_restored = copy_tree(&src.join("media"), &root.join("media"))?;
        }
        Ok(report)
    }

    fn copy_media_out(
        &self,
        root: &Path,
        dest: &Path,
        include_audio: bool,
        report: &mut ArchiveReport,
    ) -> Result<()> {
        let mut wanted: Vec<(String, bool)> = Vec::new();

        // The provider's raw response holds the *entire* transcript (§9.6). It
        // is text, not audio, so it travels whenever a data root is known --
        // leaving it behind would mean the archive silently held less than the
        // library did.
        {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT raw_response_rel_path FROM transcripts
                  WHERE raw_response_rel_path IS NOT NULL AND raw_response_rel_path <> ''",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                wanted.push((row?, false));
            }
        }
        if include_audio {
            let conn = self.conn();
            let mut stmt =
                conn.prepare("SELECT rel_path FROM recordings WHERE state = 'complete'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                wanted.push((row?, true));
            }
        }

        for (rel, is_audio) in wanted {
            let from = root.join(&rel);
            if !from.is_file() {
                report.missing_media.push(rel);
                continue;
            }
            let to = dest.join(&rel);
            if let Some(parent) = to.parent() {
                create_dir(parent)?;
            }
            let bytes = std::fs::copy(&from, &to)
                .map_err(|e| StoreError::io("copying media into the archive", &from, e))?;
            if is_audio {
                report.audio_files += 1;
                report.audio_bytes += bytes;
            }
        }
        Ok(())
    }
}

// -------------------------------------------------------------- the inserter

/// Insert every row of one table, classifying each failure.
fn insert_rows<T: TableRow + PartialEq>(
    tx: &Transaction<'_>,
    rows: &[T],
    report: &mut ImportReport,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let sql = T::insert_statement();
    for row in rows {
        let mut stmt = tx.prepare_cached(&sql)?;
        match stmt.execute(rusqlite::params_from_iter(row.values())) {
            Ok(_) => report.rows_inserted += 1,
            Err(e) if is_constraint_violation(&e) => {
                drop(stmt);
                classify(tx, row, report)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn is_constraint_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == ErrorCode::ConstraintViolation
    )
}

/// Decide what a constraint failure meant, and do the least surprising thing.
fn classify<T: TableRow + PartialEq>(
    tx: &Transaction<'_>,
    row: &T,
    report: &mut ImportReport,
) -> Result<()> {
    let identity = row.pk_display();

    // Case 1: this exact row is already here.
    if let Some(existing) = read_by_pk::<T>(tx, row)? {
        if &existing == row {
            report.rows_already_present += 1;
        } else {
            report.conflicts.push(Conflict {
                table: T::TABLE_NAME,
                id: identity,
                detail:
                    "a different row already exists with this id; the archive's version was not \
                     applied. Delete the local row first if the archive's is the one you want."
                        .to_owned(),
            });
        }
        return Ok(());
    }

    // Case 2: something else holds an exclusive flag — one default template,
    // one current summary, one primary transcript, one self device. Import the
    // row with the flag cleared and say so, rather than dropping it.
    if !T::EXCLUSIVE_FLAGS.is_empty() {
        let sql = T::insert_statement();
        let mut stmt = tx.prepare_cached(&sql)?;
        let values = row.values_with_flags_cleared();
        if stmt.execute(rusqlite::params_from_iter(values)).is_ok() {
            report.rows_inserted += 1;
            report.conflicts.push(Conflict {
                table: T::TABLE_NAME,
                id: identity,
                detail: format!(
                    "imported, but {} was cleared because this library already has one",
                    T::EXCLUSIVE_FLAGS.join("/")
                ),
            });
            return Ok(());
        }
    }

    // Case 3: an identity clash this module cannot resolve without inventing
    // data — two different people with one email, two tags with one name.
    // Continuing would leave rows pointing at a target that never arrived.
    Err(StoreError::InvalidArgument(format!(
        "cannot import {}/{identity}: it collides with a row already in this library, \
         and resolving it would mean changing your data. Import into an empty library, \
         or remove the clashing row first.",
        T::TABLE_NAME
    )))
}

fn read_by_pk<T: TableRow>(tx: &Transaction<'_>, row: &T) -> Result<Option<T>> {
    let predicate = T::PK_COLUMNS
        .iter()
        .enumerate()
        .map(|(i, c)| format!("\"{c}\" = ?{}", i + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("{}WHERE {predicate}", T::select_prefix());
    let mut stmt = tx.prepare_cached(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(row.pk_values()))?;
    match rows.next()? {
        Some(r) => Ok(Some(T::read(r)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------- filesystem

/// Refuse a destination that holds something that is not one of our archives.
fn prepare_destination(dest: &Path) -> Result<()> {
    if !dest.exists() {
        return create_dir(dest);
    }
    if !dest.is_dir() {
        return Err(StoreError::InvalidArgument(format!(
            "{} is not a directory",
            dest.display()
        )));
    }
    let mut entries = std::fs::read_dir(dest)
        .map_err(|e| StoreError::io("reading the export destination", dest, e))?
        .flatten()
        .peekable();
    if entries.peek().is_none() {
        return Ok(());
    }
    if dest.join("library.json").is_file() {
        return Ok(());
    }
    Err(StoreError::InvalidArgument(format!(
        "{} is not empty and does not look like a FlyOnTheWall archive; \
         refusing to write into it",
        dest.display()
    )))
}

/// True when the file is a readable `meeting@1` document for `id`.
///
/// Checked by parsing rather than by existence: an export killed mid-write
/// leaves a file that exists and is half a document, and "resume" that trusts
/// the filename produces an archive that fails to import much later.
fn is_intact_meeting_file(path: &Path, id: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    MeetingDoc::from_json(&text).is_ok_and(|d| d.meeting.id == id)
}

fn create_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|e| StoreError::io("creating a directory", path, e))
}

/// Write via a temporary file and a rename (EXP-04's "atomic writes").
///
/// A partially written meeting document that still carries yesterday's name is
/// the worst possible artifact: it looks complete to `resume` and to a human
/// listing the directory, and fails only at import.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| StoreError::io("writing an archive file", &tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| StoreError::io("renaming an archive file", path, e))
}

/// Copy every `*.md` from one directory to another, returning how many.
fn copy_markdown_dir(src: &Path, dest: &Path) -> Result<usize> {
    let Ok(entries) = std::fs::read_dir(src) else {
        return Ok(0);
    };
    create_dir(dest)?;
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md") {
            let to = dest.join(path.file_name().unwrap_or_default());
            std::fs::copy(&path, &to)
                .map_err(|e| StoreError::io("copying a template file", &path, e))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Recursively copy a tree, returning how many files landed.
fn copy_tree(src: &Path, dest: &Path) -> Result<usize> {
    let Ok(entries) = std::fs::read_dir(src) else {
        return Ok(0);
    };
    create_dir(dest)?;
    let mut n = 0;
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            n += copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| StoreError::io("restoring a media file", &from, e))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Every table the archive carries, for the CLI's summary line.
#[must_use]
pub fn archived_tables() -> BTreeSet<&'static str> {
    [
        DeviceRow::TABLE_NAME,
        AppMetaRow::TABLE_NAME,
        SettingRow::TABLE_NAME,
        FolderRow::TABLE_NAME,
        TemplateRow::TABLE_NAME,
        PersonRow::TABLE_NAME,
        TagRow::TABLE_NAME,
        TombstoneRow::TABLE_NAME,
        MeetingRow::TABLE_NAME,
        ParticipantRow::TABLE_NAME,
        MeetingTagRow::TABLE_NAME,
        TranscriptRow::TABLE_NAME,
        SegmentRow::TABLE_NAME,
        NoteRow::TABLE_NAME,
        NoteAnchorRow::TABLE_NAME,
        SummaryRow::TABLE_NAME,
        ActionItemRow::TABLE_NAME,
        RecordingRow::TABLE_NAME,
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_readme_states_the_thing_a_user_must_understand() {
        let lowered = PLAINTEXT_WARNING.to_lowercase();
        assert!(lowered.contains("not encrypted"));
        assert!(lowered.contains("plain text"));
        assert!(lowered.contains("deleting a meeting"));
    }

    #[test]
    fn the_archive_covers_every_table_the_schema_has() {
        // 18 tables in migration 0001, FTS excluded (derived, rebuildable --
        // §9.7 invariant 6). A new table added without an archive home fails
        // `every_column_of_every_table_appears_in_the_archive`; this is the
        // cheap unit-level version of the same guard.
        assert_eq!(archived_tables().len(), 18);
    }
}
