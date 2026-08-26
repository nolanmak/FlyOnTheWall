//! Export: the typed documents that make "no lock-in" true (EXP-01, issue #37).
//!
//! Three renderings of one meeting, and one of them is a contract:
//!
//! * **Markdown with YAML frontmatter** — for a human, and specifically for
//!   Obsidian, where the frontmatter is the only reason the file is a *note*
//!   rather than a wall of text.
//! * **Plain text** — the transcript as `[00:12:34] Alice: …`.
//! * **JSON, `flyonthewall/meeting@1`** — everything. Every segment, every word
//!   timing, every note anchor, every summary version, all provenance.
//!
//! # `meeting@1` is a public contract
//!
//! Other people's tools will read it. Within `@1` the rules are:
//!
//! * **Additive changes only.** A new field must be `#[serde(default)]` so an
//!   older archive still parses, and must never change the meaning of an
//!   existing one.
//! * **Unknown fields are ignored, not rejected**, so a document written by a
//!   newer build still opens here. What guards against genuinely losing data
//!   that way is [`crate::archive`]'s refusal to import a manifest whose
//!   `schema_version` exceeds this build's.
//! * **JSON keys are exactly the SQLite column names.** Not a stylistic
//!   choice: it is what lets `every_column_of_every_table_appears_in_the_archive`
//!   compare the document against `PRAGMA table_info` with no rename table in
//!   between, so a column added by a future migration and forgotten here fails
//!   CI instead of vanishing from every export.
//!
//! # The security tradeoff, stated plainly
//!
//! The library is SQLCipher-encrypted (§9.1). **Everything this module writes is
//! plaintext.** That is inherent — an export another tool cannot read is not an
//! export — but it means a bulk archive is an unencrypted copy of every meeting
//! the user has ever recorded, sitting in an ordinary directory with ordinary
//! permissions, outside the reach of `delete_meeting`'s byte-scrubbing (§9.6).
//! Every caller-facing surface says so: [`crate::archive`] writes a `README.txt`
//! into the archive root, the manifest carries `"encryption": "none"`, and the
//! CLI requires the user to acknowledge it before writing one.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{Row, ToSql, params_from_iter};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::db::Db;
use crate::error::{Result, StoreError};

/// The versioned document kind for a single meeting.
pub const MEETING_SCHEMA: &str = "flyonthewall/meeting@1";

/// Binary column data, carried as base64 in JSON.
///
/// A newtype rather than `Option<String>` on the struct, so that
/// [`SegmentRow::words`] stays a *blob* all the way to the SQL binding and the
/// generated insert needs no special case. It also keeps the JSON key equal to
/// the column name, which the schema-coverage lint depends on.
///
/// `Some(Blob(vec![]))` and `None` are different values and stay different: an
/// empty BLOB is a segment whose word timings are known to be empty, and NULL
/// is a segment whose word timings were never captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob(pub Vec<u8>);

impl Serialize for Blob {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&B64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Blob {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        B64.decode(s.as_bytes())
            .map(Blob)
            .map_err(serde::de::Error::custom)
    }
}

impl ToSql for Blob {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Blob(&self.0)))
    }
}

impl FromSql for Blob {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Blob(b) => Ok(Self(b.to_vec())),
            ValueRef::Null => Err(FromSqlError::InvalidType),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

/// Declare a row type once, and derive from it the column list, the reader and
/// the writer.
///
/// The three drifting apart is the failure this macro exists to prevent: a
/// hand-written `SELECT` that gained a column its `INSERT` did not is exactly
/// how a "lossless" exporter loses a field, and it is invisible at review
/// because both statements look complete on their own.
macro_rules! row_type {
    (
        $(#[$meta:meta])*
        $name:ident, $table:literal,
        pk = [$($pk:literal),* $(,)?],
        exclusive = [$($flag:literal),* $(,)?],
        { $( $(#[$fmeta:meta])* $field:ident : $ty:ty ),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub struct $name {
            $(
                // The column name IS the documentation: the JSON key, the
                // struct field and the SQLite column are the same string by
                // construction, which is the property the schema-coverage
                // lint in `tests/roundtrip.rs` checks.
                #[doc = concat!("`", $table, ".", stringify!($field), "`.")]
                $(#[$fmeta])*
                pub $field: $ty,
            )*
        }

        impl $name {
            /// The table these rows live in.
            pub const TABLE: &'static str = $table;
            /// Column names, in declaration order. Also the JSON key order.
            pub const COLUMNS: &'static [&'static str] = &[ $( stringify!($field) ),* ];
            /// The primary key, which is what "already imported" is judged on.
            pub const PK: &'static [&'static str] = &[ $( $pk ),* ];
            /// Columns governed by a partial unique index that only one row in
            /// the whole library (or the whole meeting) may set.
            pub const EXCLUSIVE: &'static [&'static str] = &[ $( $flag ),* ];

            fn select_list() -> String {
                Self::COLUMNS
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            }

            fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
                let mut i = 0usize;
                $(
                    let $field: $ty = row.get(i)?;
                    #[allow(unused_assignments)]
                    { i += 1; }
                )*
                Ok(Self { $( $field ),* })
            }

            /// `INSERT INTO <table> (...) VALUES (?1, ?2, ...)`.
            fn insert_sql() -> String {
                let placeholders = (1..=Self::COLUMNS.len())
                    .map(|n| format!("?{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "INSERT INTO \"{}\" ({}) VALUES ({placeholders})",
                    Self::TABLE,
                    Self::select_list()
                )
            }

            fn to_sql_values(&self) -> Vec<&dyn ToSql> {
                vec![ $( &self.$field as &dyn ToSql ),* ]
            }
        }
    };
}

row_type!(
    /// A row of `devices`.
    DeviceRow, "devices",
    pk = ["id"], exclusive = ["is_self"], {
        id: String,
        name: String,
        platform: String,
        app_version: String,
        is_self: i64,
        last_seen_at_ms: Option<i64>,
        created_at: i64,
        updated_at: i64,
    }
);

row_type!(
    /// A row of `app_meta`. Local provenance (§9.7), exported anyway: an
    /// archive that drops it is not a complete backup, and deciding on the
    /// user's behalf which of their rows are interesting is how fields go
    /// missing.
    AppMetaRow, "app_meta",
    pk = ["key"], exclusive = [], {
        key: String,
        value: String,
        updated_at: i64,
    }
);

row_type!(
    /// A row of `settings`.
    SettingRow, "settings",
    pk = ["key"], exclusive = [], {
        key: String,
        value: String,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `folders`.
    FolderRow, "folders",
    pk = ["id"], exclusive = [], {
        id: String,
        name: String,
        parent_id: Option<String>,
        sort_key: String,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `templates`. The database side of a template; the *files*
    /// (issue #36) travel separately in the archive's `templates/` directory.
    TemplateRow, "templates",
    pk = ["id"], exclusive = ["is_default"], {
        id: String,
        name: String,
        body_md: String,
        is_builtin: i64,
        is_default: i64,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `people`.
    PersonRow, "people",
    pk = ["id"], exclusive = [], {
        id: String,
        display_name: String,
        email: Option<String>,
        is_self: i64,
        voice_label: Option<String>,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `tags`.
    TagRow, "tags",
    pk = ["id"], exclusive = [], {
        id: String,
        name: String,
        color: Option<String>,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `tombstones`. Identity only, never content (§9.6).
    TombstoneRow, "tombstones",
    pk = ["id"], exclusive = [], {
        id: String,
        kind: String,
        deleted_at: i64,
        origin_device_id: String,
        lamport: i64,
    }
);

row_type!(
    /// A row of `meetings`.
    MeetingRow, "meetings",
    pk = ["id"], exclusive = [], {
        id: String,
        title: String,
        started_at_ms: i64,
        ended_at_ms: Option<i64>,
        duration_ms: Option<i64>,
        tz: String,
        folder_id: Option<String>,
        template_id: Option<String>,
        calendar_uid: Option<String>,
        calendar_source: Option<String>,
        meeting_url: Option<String>,
        app_hint: Option<String>,
        state: String,
        language: Option<String>,
        disclosed: i64,
        retain_audio: String,
        retain_audio_days: Option<i64>,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
        /// What the last enrichment pass found (#74). Added by migration 0003,
        /// so `#[serde(default)]`: an archive written before it must still
        /// import, per the additive rule in the module docs.
        ///
        /// This is *device-local* state — which binary the daemon on that
        /// machine could resolve — and it rides the archive anyway, because
        /// the column-coverage guard makes `meeting@1` mean "every column of
        /// `meetings`" with no exceptions anyone has to remember. The
        /// consequence, stated rather than discovered later: an import carries
        /// the exporting machine's diagnosis, and the first enrichment pass on
        /// the importing machine replaces it with the truth there.
        #[serde(default)]
        enrich_status: Option<String>,
        /// The reason behind `enrich_status`. Same additive rule, and the same
        /// untrusted-text caveat as the column it mirrors: for `failed` it is
        /// an engine's own stderr.
        #[serde(default)]
        enrich_detail: Option<String>,
    }
);

row_type!(
    /// A row of `meeting_participants`.
    ParticipantRow, "meeting_participants",
    pk = ["id"], exclusive = [], {
        id: String,
        meeting_id: String,
        person_id: Option<String>,
        display_name: String,
        email: Option<String>,
        role: Option<String>,
        speaker_label: Option<String>,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `meeting_tags`.
    MeetingTagRow, "meeting_tags",
    pk = ["meeting_id", "tag_id"], exclusive = [], {
        meeting_id: String,
        tag_id: String,
        created_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `transcripts`. Every transcript, not only the primary: §9.3
    /// keeps them all so re-transcribing does not destroy the old one, and an
    /// export that kept only the primary would undo that.
    TranscriptRow, "transcripts",
    pk = ["id"], exclusive = ["is_primary"], {
        id: String,
        meeting_id: String,
        provider: String,
        model: String,
        is_primary: i64,
        language: Option<String>,
        audio_ms: Option<i64>,
        cost_micros: Option<i64>,
        raw_response_rel_path: Option<String>,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `segments`, word timings included.
    SegmentRow, "segments",
    pk = ["id"], exclusive = [], {
        id: String,
        transcript_id: String,
        meeting_id: String,
        idx: i64,
        start_ms: i64,
        end_ms: i64,
        channel: String,
        speaker_label: Option<String>,
        person_id: Option<String>,
        text: String,
        confidence: Option<f64>,
        is_final: i64,
        /// zstd'd JSON word timings, base64 in the document.
        words: Option<Blob>,
    }
);

row_type!(
    /// A row of `notes`.
    NoteRow, "notes",
    pk = ["id"], exclusive = [], {
        id: String,
        meeting_id: String,
        body_md: String,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `note_anchors` — the alignment between a note block and the
    /// moment it was typed. Without these the augmentation feature degrades to
    /// plain summarisation, so they are load-bearing rather than incidental.
    NoteAnchorRow, "note_anchors",
    pk = ["id"], exclusive = [], {
        id: String,
        note_id: String,
        meeting_id: String,
        block_idx: i64,
        block_text: String,
        typed_at_ms: i64,
    }
);

row_type!(
    /// A row of `summaries`. Every version.
    SummaryRow, "summaries",
    pk = ["id"], exclusive = ["is_current"], {
        id: String,
        meeting_id: String,
        version: i64,
        template_id: Option<String>,
        transcript_id: Option<String>,
        provider: String,
        model: String,
        prompt_hash: String,
        body_md: String,
        coverage: Option<f64>,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        cost_micros: Option<i64>,
        is_current: i64,
        created_at: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `action_items`.
    ActionItemRow, "action_items",
    pk = ["id"], exclusive = [], {
        id: String,
        meeting_id: String,
        summary_id: Option<String>,
        kind: String,
        text: String,
        owner_person_id: Option<String>,
        owner_label: Option<String>,
        due_ms: Option<i64>,
        due_raw: Option<String>,
        confidence: String,
        evidence_segment_ids: String,
        evidence_quote: Option<String>,
        status: String,
        created_at: i64,
        updated_at: i64,
        lamport: i64,
        origin_device_id: String,
    }
);

row_type!(
    /// A row of `recordings` — the audio *metadata*. The bytes themselves are
    /// opt-in and travel under the archive's `media/` directory.
    RecordingRow, "recordings",
    pk = ["id"], exclusive = [], {
        id: String,
        meeting_id: String,
        channel: String,
        rel_path: String,
        codec: String,
        container: String,
        sample_rate: i64,
        channels: i64,
        bitrate_bps: i64,
        duration_ms: Option<i64>,
        bytes: Option<i64>,
        sha256: Option<String>,
        encrypted: i64,
        state: String,
        purge_after_ms: Option<i64>,
        created_at: i64,
        updated_at: i64,
        deleted_at: Option<i64>,
        lamport: i64,
        origin_device_id: String,
    }
);

/// Everything one meeting is.
///
/// Serialized as `flyonthewall/meeting@1`. `notes` is a list even though
/// `notes` has `UNIQUE (meeting_id)` and therefore holds at most one row: the
/// shape stays the same whether or not a note exists, which is one fewer
/// special case for a third-party reader and one fewer branch here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingDoc {
    /// Always [`MEETING_SCHEMA`].
    pub schema: String,
    /// The meeting row.
    pub meeting: MeetingRow,
    /// Attendees.
    pub participants: Vec<ParticipantRow>,
    /// Tag links.
    pub meeting_tags: Vec<MeetingTagRow>,
    /// The tag definitions those links point at, and the owning folder.
    ///
    /// Denormalised on purpose, and the only denormalised data in the format.
    /// A single-meeting export has no manifest beside it, so without these a
    /// `meeting@1` document would name its folder and its tags by UUID and
    /// nothing could resolve them -- which would make the per-meeting JSON
    /// (EXP-01) strictly less useful than the Markdown next to it.
    ///
    /// The library importer does **not** insert these: `library.json` is
    /// authoritative for tables shared between meetings, and inserting them
    /// twice would only make the idempotency counters lie.
    pub tags: Vec<TagRow>,
    /// The meeting's folder, resolved. See [`MeetingDoc::tags`].
    pub folder: Option<FolderRow>,
    /// Every transcript.
    pub transcripts: Vec<TranscriptRow>,
    /// Every segment of every transcript.
    pub segments: Vec<SegmentRow>,
    /// The note document, if there is one.
    pub notes: Vec<NoteRow>,
    /// Its anchors.
    pub note_anchors: Vec<NoteAnchorRow>,
    /// Every summary version.
    pub summaries: Vec<SummaryRow>,
    /// Extracted items.
    pub action_items: Vec<ActionItemRow>,
    /// Audio metadata.
    pub recordings: Vec<RecordingRow>,
}

/// The two flavors a clipboard write carries (EXP-02).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipboard {
    /// `text/plain` — what lands in an editor.
    pub text: String,
    /// `text/html` — what lands in Slack or Notion.
    pub html: String,
}

impl Db {
    /// Everything stored about one meeting, as [`MeetingDoc`].
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] if there is no such meeting, so that an
    /// export of a mistyped id is a failure rather than an empty file the user
    /// discovers is empty months later.
    pub fn export_meeting(&self, meeting_id: &str) -> Result<MeetingDoc> {
        let meeting = self
            .fetch_one::<MeetingRow>("WHERE id = ?1", &[&meeting_id])?
            .ok_or_else(|| StoreError::NotFound {
                kind: "meeting",
                id: meeting_id.to_owned(),
            })?;

        Ok(MeetingDoc {
            schema: MEETING_SCHEMA.to_owned(),
            meeting,
            participants: self.fetch_by_meeting("ORDER BY id", meeting_id)?,
            meeting_tags: self.fetch_by_meeting("ORDER BY tag_id", meeting_id)?,
            tags: self.fetch_where(
                "WHERE id IN (SELECT tag_id FROM meeting_tags WHERE meeting_id = ?1) ORDER BY name",
                &[&meeting_id],
            )?,
            folder: self
                .fetch_where::<FolderRow>(
                    "WHERE id = (SELECT folder_id FROM meetings WHERE id = ?1)",
                    &[&meeting_id],
                )?
                .into_iter()
                .next(),
            transcripts: self.fetch_by_meeting("ORDER BY id", meeting_id)?,
            segments: self.fetch_by_meeting("ORDER BY transcript_id, idx", meeting_id)?,
            notes: self.fetch_by_meeting("ORDER BY id", meeting_id)?,
            note_anchors: self.fetch_by_meeting("ORDER BY note_id, block_idx", meeting_id)?,
            summaries: self.fetch_by_meeting("ORDER BY version", meeting_id)?,
            action_items: self.fetch_by_meeting("ORDER BY id", meeting_id)?,
            recordings: self.fetch_by_meeting("ORDER BY channel", meeting_id)?,
        })
    }
}

/// Read and write one row type generically.
///
/// A trait rather than more macro output, so the archive writer can say
/// "select every row of this table" once instead of eighteen times.
pub(crate) trait TableRow: Sized + Serialize {
    /// The table.
    const TABLE_NAME: &'static str;
    /// Column names.
    const COLUMN_NAMES: &'static [&'static str];
    /// Primary-key column names — how "is this row already here?" is asked.
    const PK_COLUMNS: &'static [&'static str];
    /// Columns behind a partial unique index that at most one row may set:
    /// `templates.is_default`, `summaries.is_current`,
    /// `transcripts.is_primary`, `devices.is_self`. These are the constraints
    /// two independent libraries can both satisfy and not jointly satisfy, and
    /// the reason the importer has a demotion path at all.
    const EXCLUSIVE_FLAGS: &'static [&'static str];
    /// Read from a row.
    fn read(row: &Row<'_>) -> rusqlite::Result<Self>;
    /// `SELECT <cols> FROM <table>`.
    fn select_prefix() -> String;
    /// `INSERT INTO <table> (<cols>) VALUES (?1, ...)`.
    fn insert_statement() -> String;
    /// Bindable values, in column order.
    fn values(&self) -> Vec<&dyn ToSql>;

    /// The primary-key values, in `PK_COLUMNS` order.
    ///
    /// # Panics
    ///
    /// If a declared primary-key column is not one of the row's columns, which
    /// is a typo in the declaration and is caught by the first test that
    /// touches the table.
    fn pk_values(&self) -> Vec<&dyn ToSql> {
        let all = self.values();
        Self::PK_COLUMNS
            .iter()
            .map(|pk| {
                let i = Self::COLUMN_NAMES
                    .iter()
                    .position(|c| c == pk)
                    .unwrap_or_else(|| {
                        panic!("{}: primary key `{pk}` is not a column", Self::TABLE_NAME)
                    });
                all[i]
            })
            .collect()
    }

    /// The same values, with every exclusive flag forced to 0.
    fn values_with_flags_cleared(&self) -> Vec<&dyn ToSql> {
        static ZERO: i64 = 0;
        let mut all = self.values();
        for flag in Self::EXCLUSIVE_FLAGS {
            if let Some(i) = Self::COLUMN_NAMES.iter().position(|c| c == flag) {
                all[i] = &ZERO;
            }
        }
        all
    }

    /// The row's identity as text, for a report a human reads.
    fn pk_display(&self) -> String {
        let v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        Self::PK_COLUMNS
            .iter()
            .map(|c| match v.get(*c) {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}

macro_rules! impl_table_row {
    ($($name:ident),* $(,)?) => {
        $(
            impl TableRow for $name {
                const TABLE_NAME: &'static str = Self::TABLE;
                const COLUMN_NAMES: &'static [&'static str] = Self::COLUMNS;
                const PK_COLUMNS: &'static [&'static str] = Self::PK;
                const EXCLUSIVE_FLAGS: &'static [&'static str] = Self::EXCLUSIVE;
                fn read(row: &Row<'_>) -> rusqlite::Result<Self> { Self::from_row(row) }
                fn select_prefix() -> String {
                    format!("SELECT {} FROM \"{}\" ", Self::select_list(), Self::TABLE)
                }
                fn insert_statement() -> String { Self::insert_sql() }
                fn values(&self) -> Vec<&dyn ToSql> { self.to_sql_values() }
            }
        )*
    };
}

impl_table_row!(
    DeviceRow,
    AppMetaRow,
    SettingRow,
    FolderRow,
    TemplateRow,
    PersonRow,
    TagRow,
    TombstoneRow,
    MeetingRow,
    ParticipantRow,
    MeetingTagRow,
    TranscriptRow,
    SegmentRow,
    NoteRow,
    NoteAnchorRow,
    SummaryRow,
    ActionItemRow,
    RecordingRow,
);

impl Db {
    pub(crate) fn fetch_all<T: TableRow>(&self, suffix: &str) -> Result<Vec<T>> {
        self.fetch_where::<T>(suffix, &[])
    }

    pub(crate) fn fetch_where<T: TableRow>(
        &self,
        suffix: &str,
        params: &[&dyn ToSql],
    ) -> Result<Vec<T>> {
        let sql = format!("{}{suffix}", T::select_prefix());
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |r| T::read(r))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn fetch_one<T: TableRow>(&self, suffix: &str, params: &[&dyn ToSql]) -> Result<Option<T>> {
        Ok(self.fetch_where::<T>(suffix, params)?.into_iter().next())
    }

    fn fetch_by_meeting<T: TableRow>(&self, order: &str, meeting_id: &str) -> Result<Vec<T>> {
        self.fetch_where::<T>(&format!("WHERE meeting_id = ?1 {order}"), &[&meeting_id])
    }
}

// ------------------------------------------------------------------ renderers

impl MeetingDoc {
    /// Serialize as pretty-printed `meeting@1`.
    ///
    /// # Panics
    ///
    /// Never in practice: every field is a plain scalar or a `Vec` of them, and
    /// `serde_json` only fails on maps with non-string keys or on a `f64` that
    /// is NaN — neither of which SQLite can produce here.
    #[must_use]
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("meeting@1 is always serializable")
    }

    /// Parse a `meeting@1` document.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidArgument`] with the JSON parser's message, and a
    /// refusal if the `schema` field names a document kind this build does not
    /// understand — guessing at an unknown format is how half a meeting gets
    /// imported.
    pub fn from_json(text: &str) -> Result<Self> {
        let doc: Self = serde_json::from_str(text)
            .map_err(|e| StoreError::InvalidArgument(format!("not a meeting@1 document: {e}")))?;
        if doc.schema != MEETING_SCHEMA {
            return Err(StoreError::InvalidArgument(format!(
                "expected a `{MEETING_SCHEMA}` document, found `{}`",
                doc.schema
            )));
        }
        Ok(doc)
    }

    /// The summary the UI shows, if there is one.
    #[must_use]
    pub fn current_summary(&self) -> Option<&SummaryRow> {
        self.summaries
            .iter()
            .find(|s| s.is_current != 0)
            .or_else(|| self.summaries.iter().max_by_key(|s| s.version))
    }

    /// A one-line description for OKF frontmatter.
    ///
    /// The first substantive line of the summary — skipping headings and
    /// blank lines — bounded so the frontmatter stays a scalar; the title
    /// when there is no summary yet; a constant when there is neither.
    fn okf_description(&self) -> String {
        const BUDGET: usize = 200;
        let from_summary = self.current_summary().and_then(|s| {
            s.body_md
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
                .map(ToOwned::to_owned)
        });
        let text = from_summary
            .filter(|s| !s.is_empty())
            .or_else(|| Some(self.meeting.title.clone()).filter(|t| !t.is_empty()))
            .unwrap_or_else(|| "Meeting transcript".to_owned());
        if text.chars().count() <= BUDGET {
            return text;
        }
        let cut: String = text.chars().take(BUDGET - 1).collect();
        let cut = cut.rfind(' ').map_or(cut.as_str(), |i| &cut[..i]);
        format!("{cut}…")
    }

    /// Markdown with YAML frontmatter — a valid Obsidian note (EXP-01).
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let m = &self.meeting;
        let mut out = String::from("---\n");
        // `type` is the one field OKF (Google's Open Knowledge Format) requires
        // on every concept, so a repo of these files is an agent-consumable
        // bundle rather than a pile of notes. It is additive YAML that Obsidian
        // ignores, so the same file is still a valid note (EXP-01).
        out.push_str("type: meeting-transcript\n");
        out.push_str(&format!("id: {}\n", yaml_scalar(&m.id)));
        out.push_str(&format!("title: {}\n", yaml_scalar(&m.title)));
        // OKF's recommended one-line summary — the first real line of the
        // generated summary, or the title, so a listing can show what the
        // meeting was about without opening it.
        out.push_str(&format!(
            "description: {}\n",
            yaml_scalar(&self.okf_description())
        ));
        out.push_str(&format!(
            "date: {}\n",
            yaml_scalar(&iso_date(m.started_at_ms))
        ));
        out.push_str(&format!("started_at_ms: {}\n", m.started_at_ms));
        out.push_str(&format!(
            "duration: {}\n",
            yaml_scalar(&hms(m.duration_ms.unwrap_or(0)))
        ));
        out.push_str(&format!("timezone: {}\n", yaml_scalar(&m.tz)));
        out.push_str("attendees:\n");
        for p in &self.participants {
            out.push_str(&format!("  - {}\n", yaml_scalar(&p.display_name)));
        }
        // Names, not UUIDs: an Obsidian note tagged `018f-...` is a note with
        // no tags. `tags` above carries the definitions so this resolves even
        // for a single meeting exported on its own.
        out.push_str("tags:\n");
        for t in &self.tags {
            out.push_str(&format!("  - {}\n", yaml_scalar(&t.name)));
        }
        out.push_str(&format!(
            "folder: {}\n",
            yaml_scalar(self.folder.as_ref().map_or("", |f| f.name.as_str()))
        ));
        // §11: whether participants were told is part of the record, so it
        // travels with the note rather than living only in the app.
        out.push_str(&format!("disclosed: {}\n", m.disclosed != 0));
        // OKF provenance: who produced the derived content and when. Only when
        // a summary exists — a transcript-only export was not model-generated,
        // and claiming otherwise would be the dishonest kind of metadata.
        if let Some(s) = self.current_summary() {
            let by = if s.model.trim().is_empty() {
                s.provider.clone()
            } else {
                format!("{} {}", s.provider, s.model)
            };
            out.push_str("generated:\n");
            out.push_str(&format!("  by: {}\n", yaml_scalar(&by)));
            out.push_str(&format!("  at: {}\n", yaml_scalar(&iso_date(s.created_at))));
        }
        out.push_str("---\n\n");

        out.push_str(&format!(
            "# {}\n\n",
            if m.title.is_empty() {
                "Untitled meeting"
            } else {
                &m.title
            }
        ));

        if let Some(s) = self.current_summary() {
            out.push_str(s.body_md.trim_end());
            out.push_str("\n\n");
        }

        if let Some(note) = self.notes.first()
            && !note.body_md.trim().is_empty()
        {
            out.push_str("## Notes\n\n");
            out.push_str(note.body_md.trim_end());
            out.push_str("\n\n");
        }

        if !self.action_items.is_empty() {
            out.push_str("## Action items\n\n");
            for item in &self.action_items {
                let owner = item
                    .owner_label
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map_or(String::new(), |o| format!(" — {o}"));
                let done = if item.status == "done" { "x" } else { " " };
                out.push_str(&format!("- [{done}] {}{owner}\n", item.text));
            }
            out.push('\n');
        }

        if !self.segments.is_empty() {
            out.push_str("## Transcript\n\n");
            out.push_str(&self.transcript_lines("- "));
            out.push('\n');
        }
        out
    }

    /// The transcript as `[00:12:34] Alice: …` (issue #37).
    #[must_use]
    pub fn to_plain_text(&self) -> String {
        let m = &self.meeting;
        let mut out = String::new();
        out.push_str(&format!(
            "{}\n{}\n\n",
            if m.title.is_empty() {
                "Untitled meeting"
            } else {
                &m.title
            },
            iso_date(m.started_at_ms)
        ));
        out.push_str(&self.transcript_lines(""));
        out
    }

    /// `[00:12:34] Alice: …`, one segment per line.
    ///
    /// `prefix` is `"- "` for Markdown and empty for plain text, and that is
    /// not cosmetic: CommonMark joins consecutive lines into one paragraph, so
    /// a Markdown transcript written as bare lines renders in Obsidian as a
    /// single wall of text with the timestamps buried mid-sentence. List items
    /// are the only form that survives every renderer.
    fn transcript_lines(&self, prefix: &str) -> String {
        // Only the primary transcript is rendered for humans -- the others are
        // alternative takes on the same audio and interleaving them would read
        // as the meeting having happened twice. Every one of them is still in
        // the JSON, which is where losslessness lives.
        let primary = self
            .transcripts
            .iter()
            .find(|t| t.is_primary != 0)
            .or_else(|| self.transcripts.first());
        let Some(primary) = primary else {
            return String::new();
        };

        let mut lines = String::new();
        for s in self
            .segments
            .iter()
            .filter(|s| s.transcript_id == primary.id)
        {
            let who = s
                .speaker_label
                .as_deref()
                .filter(|l| !l.is_empty())
                .unwrap_or("Speaker");
            // A segment's text can itself contain a newline (STT providers
            // emit them), which would break out of the list item. One line in,
            // one line out.
            let text = s.text.replace(['\n', '\r'], " ");
            lines.push_str(&format!("{prefix}[{}] {who}: {text}\n", hms(s.start_ms)));
        }
        lines
    }

    /// Both clipboard flavors (EXP-02).
    #[must_use]
    pub fn to_clipboard(&self) -> Clipboard {
        let m = &self.meeting;
        let title = if m.title.is_empty() {
            "Untitled meeting"
        } else {
            &m.title
        };
        let mut text = format!("{title}\n\n");
        let mut html = format!("<h1>{}</h1>\n", escape_html(title));

        if let Some(s) = self.current_summary() {
            text.push_str(s.body_md.trim_end());
            text.push_str("\n\n");
            html.push_str(&markdown_to_html(&s.body_md));
        }
        if !self.action_items.is_empty() {
            text.push_str("Action items\n");
            html.push_str("<h2>Action items</h2>\n<ul>\n");
            for item in &self.action_items {
                let owner = item
                    .owner_label
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map_or(String::new(), |o| format!(" — {o}"));
                text.push_str(&format!("- {}{owner}\n", item.text));
                html.push_str(&format!(
                    "<li>{}{}</li>\n",
                    escape_html(&item.text),
                    escape_html(&owner)
                ));
            }
            html.push_str("</ul>\n");
        }
        Clipboard { text, html }
    }
}

/// A deliberately small Markdown subset: headings, bullets, paragraphs.
///
/// Not a Markdown library, and not because one would be hard to add. HTML on a
/// clipboard is *live markup* in whatever application receives it, and the
/// input is a model-generated document about a meeting that may itself contain
/// pasted HTML. Everything here goes through [`escape_html`] first and the
/// structure is added afterwards, so there is no path by which source text can
/// become a tag. A general converter's job is the opposite of that.
fn markdown_to_html(md: &str) -> String {
    let mut out = String::new();
    let mut in_list = false;
    for raw in md.lines() {
        let line = raw.trim_end();
        let (marker, rest) = if let Some(r) = line.strip_prefix("- ") {
            ("li", r)
        } else if let Some(r) = line.strip_prefix("* ") {
            ("li", r)
        } else if let Some(r) = line.strip_prefix("### ") {
            ("h3", r)
        } else if let Some(r) = line.strip_prefix("## ") {
            ("h2", r)
        } else if let Some(r) = line.strip_prefix("# ") {
            ("h1", r)
        } else if line.trim().is_empty() {
            ("", "")
        } else {
            ("p", line)
        };

        if marker != "li" && in_list {
            out.push_str("</ul>\n");
            in_list = false;
        }
        match marker {
            "" => {}
            "li" => {
                if !in_list {
                    out.push_str("<ul>\n");
                    in_list = true;
                }
                out.push_str(&format!("<li>{}</li>\n", escape_html(rest)));
            }
            tag => out.push_str(&format!("<{tag}>{}</{tag}>\n", escape_html(rest))),
        }
    }
    if in_list {
        out.push_str("</ul>\n");
    }
    out
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render a value as a YAML scalar that survives a round trip.
///
/// Always double-quoted with JSON escaping, which is a strict subset of YAML's
/// double-quoted style. The alternative — quote only when it looks necessary —
/// is a rule with a long tail (`yes`, `no`, `on`, `null`, `~`, `1.0`, a leading
/// `@`, a trailing space, anything with a `:` in it) and every miss produces an
/// Obsidian note whose frontmatter silently fails to parse.
fn yaml_scalar(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

/// Read back a scalar written by [`yaml_scalar`]. Public so the export tests
/// can assert the round trip rather than eyeballing the quoting.
///
/// # Panics
///
/// Never: a non-JSON input is returned as-is.
#[must_use]
pub fn parse_yaml_scalar(s: &str) -> String {
    serde_json::from_str::<String>(s.trim()).unwrap_or_else(|_| s.trim().to_owned())
}

/// `HH:MM:SS` from milliseconds, saturating rather than panicking on a
/// nonsense duration.
fn hms(ms: i64) -> String {
    let total = ms.max(0) / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

/// `YYYY-MM-DD` UTC from epoch milliseconds.
///
/// Hand-rolled because the alternative is a date crate for one format string,
/// and the civil-from-days algorithm is exact for the whole i64 range that
/// matters here — including the pre-1970 timestamps a corrected meeting start
/// can produce, which is where a naive `ms / 86_400_000` is off by a day.
fn iso_date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`, days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_formats_and_saturates() {
        assert_eq!(hms(0), "00:00:00");
        assert_eq!(hms(754_000), "00:12:34");
        assert_eq!(hms(3_600_000), "01:00:00");
        assert_eq!(hms(-5), "00:00:00");
        assert_eq!(hms(90_061_000), "25:01:01");
    }

    #[test]
    fn iso_date_handles_pre_epoch_and_leap_years() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(-1), "1969-12-31");
        assert_eq!(iso_date(-86_400_000), "1969-12-31");
        assert_eq!(iso_date(1_700_000_000_000), "2023-11-14");
        // 2024-02-29 00:00:00 UTC.
        assert_eq!(iso_date(1_709_164_800_000), "2024-02-29");
        assert_eq!(iso_date(4_102_444_800_000), "2100-01-01");
    }

    #[test]
    fn yaml_scalars_round_trip_through_the_nasty_cases() {
        for s in [
            "",
            "plain",
            "yes",
            "no",
            "null",
            "~",
            "1.0",
            "Q3: planning",
            "trailing space ",
            "with \"quotes\"",
            "with\nnewline",
            "with\ttab",
            "café",
            "cafe\u{301}",
            "👩‍💻",
            "- looks like a list",
            "#comment",
            "@reserved",
        ] {
            let written = yaml_scalar(s);
            assert!(
                !written.contains('\n'),
                "{s:?} produced a multi-line scalar"
            );
            assert_eq!(
                parse_yaml_scalar(&written),
                s,
                "round trip failed for {s:?}"
            );
        }
    }

    #[test]
    fn html_conversion_never_emits_source_markup() {
        let html = markdown_to_html("# <script>x</script>\n- a & b\n\nplain <b>text</b>");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<b>text"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("&amp;"), "{html}");
        assert!(html.contains("<h1>"), "{html}");
        assert!(html.contains("<ul>") && html.contains("</ul>"), "{html}");
    }

    #[test]
    fn every_float_the_schema_can_hold_survives_the_json_round_trip_bit_for_bit() {
        // `segments.confidence` and `summaries.coverage` are REAL columns, and
        // a confidence that comes back one ULP different is a lossy export
        // that no eyeball comparison would ever catch. Compared by bits, so
        // `-0.0` is not `0.0` and `0.9999999999999999` is not `1.0`.
        for v in [
            0.0f64,
            -0.0,
            1.0,
            0.1,
            0.1 + 0.2,
            0.7,
            0.9999999999999999,
            f64::MIN_POSITIVE,
            5e-324,
            f64::MAX,
            -f64::MAX,
            1e-300,
            123_456_789.123_456_79,
        ] {
            let json = serde_json::to_string(&Some(v)).unwrap();
            let back: Option<f64> = serde_json::from_str(&json).unwrap();
            assert_eq!(
                back.unwrap().to_bits(),
                v.to_bits(),
                "{v} ({json}) did not survive: {:016x} -> {:016x}",
                v.to_bits(),
                back.unwrap().to_bits()
            );
        }
    }

    #[test]
    fn base64_round_trips_the_bytes_that_are_not_text() {
        for bytes in [
            vec![],
            vec![0u8],
            vec![0xff, 0x00, 0xfe],
            (0..=255u8).collect(),
        ] {
            let b = Blob(bytes.clone());
            let json = serde_json::to_string(&b).unwrap();
            let back: Blob = serde_json::from_str(&json).unwrap();
            assert_eq!(back.0, bytes);
        }
    }

    #[test]
    fn the_generated_sql_names_every_column_once() {
        // The macro is the single source of truth for reads and writes alike;
        // this is what stops a future edit from adding a column to one half.
        let insert = SegmentRow::insert_sql();
        for c in SegmentRow::COLUMNS {
            assert_eq!(
                insert.matches(&format!("\"{c}\"")).count(),
                1,
                "column {c} appears wrong number of times in {insert}"
            );
        }
        assert!(insert.contains(&format!("?{}", SegmentRow::COLUMNS.len())));
        assert!(!insert.contains(&format!("?{}", SegmentRow::COLUMNS.len() + 1)));
    }
}
