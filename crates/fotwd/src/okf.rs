//! The OKF (Google's Open Knowledge Format) bundle layout, shared by every
//! target that writes one.
//!
//! The transcript files themselves are rendered by
//! [`fotw_store::MeetingDoc::to_markdown`] — this module owns the rest of the
//! bundle: the stable file names, the `index.md` progressive-disclosure
//! listing, and the `log.md` change history. Both the GitHub export
//! ([`crate::github`]) and the local folder export ([`crate::main`]'s
//! `export-okf`) build the same bundle from it, so the two cannot drift.

use std::path::Path;

use fotw_store::Db;

/// Write the whole library as a local OKF bundle under `dest`: one markdown
/// file per meeting, plus `index.md` and `log.md`. Returns how many meetings
/// were written.
///
/// The point is that an agent can index the folder directly — a Filesystem
/// MCP server (OpenClaw) or `qmd` (Hermes) over `dest` — for context of the
/// user's meetings, with no GitHub round trip. A meeting that cannot be
/// exported is skipped loudly rather than failing the whole run.
///
/// # Errors
///
/// A filesystem error creating `dest` or writing a file, or a store error
/// listing meetings.
pub fn export_bundle(db: &mut Db, dest: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dest)?;

    // Page through every meeting, newest first — `list` caps at 200, so a
    // busy library must not stop at the first page.
    let mut entries: Vec<BundleEntry> = Vec::new();
    let mut offset = 0;
    loop {
        let page = db
            .meetings()
            .list(200, offset)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let full = page.len() == 200;
        for m in &page {
            let Ok(doc) = db.export_meeting(&m.id) else {
                eprintln!("  ! skipped {}: could not export", m.id);
                continue;
            };
            let filename = transcript_filename(m.started_at_ms, &m.title, &m.id);
            std::fs::write(dest.join(&filename), doc.to_markdown())?;
            entries.push(BundleEntry {
                filename,
                title: m.title.clone(),
                started_at_ms: u64::try_from(m.started_at_ms).unwrap_or(0),
                // A local snapshot has no push history; the meeting's own date
                // is the honest "added" date for the change log.
                logged_at_ms: u64::try_from(m.started_at_ms).unwrap_or(0),
            });
        }
        if !full {
            break;
        }
        offset += 200;
    }

    std::fs::write(dest.join("index.md"), render_index(&entries))?;
    std::fs::write(dest.join("log.md"), render_log(&entries))?;
    Ok(entries.len())
}

/// One entry in a bundle listing.
#[derive(Debug, Clone)]
pub struct BundleEntry {
    /// The file name inside the bundle directory (no directory prefix).
    pub filename: String,
    /// The meeting title, for the link label.
    pub title: String,
    /// Meeting start, epoch milliseconds — orders and dates the index.
    pub started_at_ms: u64,
    /// When this entry was added to the bundle, epoch milliseconds — the
    /// date it falls under in `log.md`. For a GitHub push that is the push
    /// time; for a local snapshot it is the meeting's own start.
    pub logged_at_ms: u64,
}

/// The stable file name for a meeting's transcript: `YYYY-MM-DD-slug-id.md`.
///
/// Deterministic from fields that exist at export time, with the id fragment
/// carrying uniqueness — two "Standup" meetings on the same day must not fight
/// over one name.
#[must_use]
pub fn transcript_filename(started_at_ms: i64, title: &str, meeting_id: &str) -> String {
    let (y, m, d) = ymd_utc(started_at_ms);
    let id8: String = meeting_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    format!("{y:04}-{m:02}-{d:02}-{}-{id8}.md", slug(title))
}

/// The bundle's `index.md`: an OKF progressive-disclosure listing, newest
/// first, linking each transcript relatively so the graph survives a move.
///
/// Per the OKF spec, `index.md` frontmatter carries only `okf_version`.
#[must_use]
pub fn render_index(entries: &[BundleEntry]) -> String {
    let mut sorted: Vec<&BundleEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| (std::cmp::Reverse(e.started_at_ms), e.filename.clone()));

    let mut out = String::from("---\nokf_version: \"0.2\"\n---\n\n# Meeting transcripts\n\n");
    for e in sorted {
        out.push_str(&format!(
            "- [{}](./{}) — {}\n",
            display_title(&e.title),
            e.filename,
            iso_date(e.started_at_ms)
        ));
    }
    out
}

/// The bundle's `log.md`: OKF change history under ISO-8601 date headings,
/// most recent day first.
#[must_use]
pub fn render_log(entries: &[BundleEntry]) -> String {
    let mut sorted: Vec<&BundleEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.logged_at_ms));

    let mut out = String::from("# Change log\n\n");
    let mut current_day = String::new();
    for e in sorted {
        let day = iso_date(e.logged_at_ms);
        if day != current_day {
            out.push_str(&format!("## {day}\n\n"));
            current_day = day;
        }
        out.push_str(&format!(
            "- Added [{}](./{})\n",
            display_title(&e.title),
            e.filename
        ));
    }
    out
}

/// A never-empty link label.
fn display_title(title: &str) -> &str {
    let t = title.trim();
    if t.is_empty() { "Untitled meeting" } else { t }
}

/// `YYYY-MM-DD` (UTC) from epoch milliseconds, for OKF's date fields.
#[must_use]
pub fn iso_date(epoch_ms: u64) -> String {
    let (y, m, d) = ymd_utc(i64::try_from(epoch_ms).unwrap_or(0));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Lowercased, everything unsafe collapsed to `-`, bounded, never empty.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.chars().count() >= 48 {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "meeting".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Civil date from epoch milliseconds, UTC. Hinnant's `civil_from_days`,
/// exact over the whole proleptic Gregorian calendar — no leap-year table to
/// get wrong.
fn ymd_utc(epoch_ms: i64) -> (i64, u32, u32) {
    let days = epoch_ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 { y + 1 } else { y },
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(filename: &str, title: &str, started: u64, logged: u64) -> BundleEntry {
        BundleEntry {
            filename: filename.to_owned(),
            title: title.to_owned(),
            started_at_ms: started,
            logged_at_ms: logged,
        }
    }

    #[test]
    fn the_filename_is_date_slug_and_id_fragment() {
        assert_eq!(
            transcript_filename(1_755_734_400_000, "Weekly Standup", "01890c2a-ffff-7000"),
            "2025-08-21-weekly-standup-01890c2a.md"
        );
        assert_eq!(
            transcript_filename(0, "", "abc"),
            "1970-01-01-meeting-abc.md"
        );
    }

    #[test]
    fn a_slug_survives_hostile_titles() {
        assert_eq!(slug("Q3 // planning: part 2!"), "q3-planning-part-2");
        assert_eq!(slug(""), "meeting");
        assert_eq!(slug("---"), "meeting");
        assert!(slug(&"x".repeat(500)).chars().count() <= 48);
    }

    #[test]
    fn the_date_math_is_right_where_it_matters() {
        assert_eq!(ymd_utc(0), (1970, 1, 1));
        assert_eq!(ymd_utc(1_755_734_400_000), (2025, 8, 21));
        assert_eq!(ymd_utc(1_709_164_800_000), (2024, 2, 29)); // leap day
        assert_eq!(ymd_utc(-1), (1969, 12, 31)); // div_euclid, not toward zero
    }

    #[test]
    fn the_index_is_okf_and_newest_first_with_relative_links() {
        let entries = vec![
            entry(
                "2025-08-20-planning-b.md",
                "Planning",
                1_755_648_000_000,
                20,
            ),
            entry("2025-08-21-standup-a.md", "Standup", 1_755_734_400_000, 10),
        ];
        let md = render_index(&entries);
        assert!(md.starts_with("---\nokf_version:"), "{md}");
        assert_eq!(md.matches("---").count(), 2, "one frontmatter block only");
        assert!(md.contains("- [Standup](./2025-08-21-standup-a.md) — 2025-08-21"));
        assert!(md.contains("- [Planning](./2025-08-20-planning-b.md) — 2025-08-20"));
        assert!(md.find("Standup").unwrap() < md.find("Planning").unwrap());
    }

    #[test]
    fn the_log_groups_under_iso_headings_newest_day_first() {
        let entries = vec![
            entry("m-old.md", "Old", 1, 1_755_648_000_000),
            entry("m-new.md", "New", 2, 1_755_734_400_000),
        ];
        let md = render_log(&entries);
        assert!(md.contains("## 2025-08-21"));
        assert!(md.contains("## 2025-08-20"));
        assert!(md.contains("- Added [New](./m-new.md)"));
        assert!(md.find("2025-08-21").unwrap() < md.find("2025-08-20").unwrap());
    }

    #[test]
    fn a_blank_title_is_never_an_empty_label() {
        let entries = vec![entry("x.md", "   ", 1, 1)];
        assert!(render_index(&entries).contains("[Untitled meeting]"));
        assert!(render_log(&entries).contains("[Untitled meeting]"));
    }

    #[test]
    fn export_bundle_writes_a_folder_of_transcripts_plus_index_and_log() {
        use fotw_store::{DbKey, NewMeeting, NewSegment};

        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x01; 32])).unwrap();
        let id = db
            .meetings()
            .create(
                NewMeeting::new("dev-1", "UTC")
                    .title("Weekly Standup")
                    .started_at_ms(1_755_734_400_000),
            )
            .unwrap();
        let tid = db
            .meetings()
            .create_transcript(&id, "deepgram", "nova-3", true)
            .unwrap();
        db.meetings()
            .append_segments(&tid, &[NewSegment::new(0, 0, 1_500, "hello world")])
            .unwrap();
        db.meetings().set_state(&id, "ready").unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        let count = export_bundle(&mut db, dir.path()).unwrap();
        assert_eq!(count, 1);

        // The transcript file, named the same as the GitHub path would be.
        let name = transcript_filename(1_755_734_400_000, "Weekly Standup", &id);
        assert!(name.starts_with("2025-08-21-weekly-standup-"));
        let transcript = std::fs::read_to_string(dir.path().join(&name)).unwrap();
        assert!(
            transcript.contains("type: meeting-transcript"),
            "OKF frontmatter"
        );
        assert!(transcript.contains("hello world"), "the transcript body");

        // The bundle files.
        let index = std::fs::read_to_string(dir.path().join("index.md")).unwrap();
        assert!(index.contains("okf_version"));
        assert!(index.contains(&format!("[Weekly Standup](./{name})")));
        let log = std::fs::read_to_string(dir.path().join("log.md")).unwrap();
        assert!(log.contains("# Change log"));
        assert!(log.contains(&format!("[Weekly Standup](./{name})")));
    }
}
