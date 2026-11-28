//! The release renderer (D9, 5.4).
//!
//! The single source for the stored subject, body and attachment, the review
//! input and the index map. Pure: no model, no database. Transcript text is
//! copied from `input.segments` for the retained indices only, so an omitted
//! segment, or one from another meeting, cannot appear (D6, OT2). It is not the
//! browser `sharingMarkdown` function and never includes the sharing brief.

use fotw_store::export::TranscriptClock;

use super::cut::segment;
use super::summary::plain;
use super::types::{
    Bullet, IndexMap, Payload, PrepareError, PrepareInput, ReleaseSummary, Section,
};

/// The largest body that may be sent, in bytes (D9).
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// The longest subject, in characters (D9).
const MAX_SUBJECT_CHARS: usize = 150;
/// The longest attachment slug, in characters (D9).
const MAX_SLUG_CHARS: usize = 60;

/// Render the exact outgoing email for `retained`.
///
/// # Errors
///
/// [`PrepareError::BodyTooLarge`] when the body exceeds 64 KB.
pub fn render(
    input: &PrepareInput,
    retained: &[usize],
    summary: &ReleaseSummary,
) -> Result<Payload, PrepareError> {
    let (body, bullets) = body(summary);
    if body.len() > MAX_BODY_BYTES {
        return Err(PrepareError::BodyTooLarge);
    }
    let zoned = zoned(input.started_at_ms, &input.tz);
    let date = zoned.strftime("%Y-%m-%d").to_string();
    let (attachment, attachment_lines) = attachment(input, retained, &zoned);
    Ok(Payload {
        subject: subject(&input.event_title, &date),
        body,
        attachment_name: format!("{date}-{}.md", slug(&input.event_title)),
        attachment,
        index_map: IndexMap {
            attachment_lines,
            bullets,
        },
    })
}

/// The meeting start in its zone. An unknown zone falls back to UTC, which the
/// date line names, rather than failing the release.
fn zoned(started_at_ms: i64, tz: &str) -> jiff::Zoned {
    let at =
        jiff::Timestamp::from_millisecond(started_at_ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    let zone = jiff::tz::TimeZone::get(tz).unwrap_or(jiff::tz::TimeZone::UTC);
    at.to_zoned(zone)
}

/// The calendar title on one line: control characters, newlines and line
/// separators become spaces; `Meeting` when nothing is left.
fn one_line_title(title: &str) -> String {
    let line: String = title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let line = line.trim();
    if line.is_empty() {
        "Meeting".to_owned()
    } else {
        line.to_owned()
    }
}

/// `Notes: <event_title> (<YYYY-MM-DD>)`, at most 150 characters. A long title
/// is shortened so the date always survives.
fn subject(title: &str, date: &str) -> String {
    let room = MAX_SUBJECT_CHARS - "Notes:  ()".chars().count() - date.chars().count();
    let title: String = one_line_title(title).chars().take(room).collect();
    format!("Notes: {} ({date})", title.trim_end())
}

/// D9: lowercased, runs of non-`[a-z0-9]` become `-`, trimmed, at most 60
/// characters, `meeting` if empty.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(MAX_SLUG_CHARS).collect();
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "meeting".to_owned()
    } else {
        out.to_owned()
    }
}

/// The plain-text body: the D7 sections, citations stripped. Headings are
/// exactly `Summary`, `Decisions`, `Action items`, each on its own line with
/// `- ` bullets; bullet ordinals are zero-based within their section.
fn body(summary: &ReleaseSummary) -> (String, Vec<(Section, usize, Vec<usize>)>) {
    let mut out = String::new();
    let mut map = Vec::new();
    for (section, heading, bullets) in [
        (Section::Summary, "Summary", &summary.summary),
        (Section::Decisions, "Decisions", &summary.decisions),
        (Section::ActionItems, "Action items", &summary.action_items),
    ] {
        // Summary always shows; an empty Decisions or Action items is left out
        // (the Outbox parses these exact headings, #345).
        if bullets.is_empty() && section != Section::Summary {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(heading);
        out.push('\n');
        for (ordinal, Bullet { text, citations }) in bullets.iter().enumerate() {
            out.push_str("- ");
            out.push_str(&plain(text));
            out.push('\n');
            map.push((section, ordinal, citations.clone()));
        }
    }
    (out, map)
}

/// The Markdown attachment and, for each retained segment, its zero-based,
/// inclusive line range.
fn attachment(
    input: &PrepareInput,
    retained: &[usize],
    zoned: &jiff::Zoned,
) -> (String, Vec<(usize, usize, usize)>) {
    let len = input.segments.len();
    let mut kept: Vec<usize> = retained
        .iter()
        .copied()
        .filter(|i| segment(input, *i).is_some())
        .collect();
    kept.sort_unstable();
    kept.dedup();

    let corrections: Vec<fotw_web::documents::NameCorrection> = input
        .corrections
        .iter()
        .map(|c| fotw_web::documents::NameCorrection {
            from: c.from.clone(),
            to: format!("[{}]", c.to),
        })
        .collect();

    let mut doc = Doc::default();
    doc.block(&format!("# Notes: {}", one_line_title(&input.event_title)));
    doc.block(&format!(
        "Meeting: {} ({})",
        zoned.strftime("%Y-%m-%d %H:%M %Z"),
        zoned.time_zone().iana_name().unwrap_or("UTC")
    ));
    let clock = TranscriptClock::new(input.started_at_ms);
    let mut map = Vec::new();
    let mut next = 0;
    for index in kept {
        if index > next {
            doc.block(&format!("*[… {} segments omitted …]*", index - next));
        }
        let Some(s) = segment(input, index) else {
            continue;
        };
        let text = fotw_web::documents::corrected_names(&s.text, &corrections);
        let offset = i64::try_from(s.offset_ms).unwrap_or(i64::MAX);
        let stamp = clock.label(offset, offset);
        let (first, last) = doc.block(&format!("**{stamp} · {}** {text}", s.speaker));
        map.push((first, last, index));
        next = index + 1;
    }
    if next < len {
        doc.block("*[… remainder omitted …]*");
    }
    (doc.text, map)
}

/// Markdown blocks separated by blank lines, tracking line numbers.
#[derive(Default)]
struct Doc {
    text: String,
    line: usize,
}

impl Doc {
    /// Append one block; returns its first and last line.
    fn block(&mut self, block: &str) -> (usize, usize) {
        if !self.text.is_empty() {
            self.text.push('\n');
            self.line += 1;
        }
        let first = self.line;
        self.text.push_str(block);
        self.text.push('\n');
        self.line += block.matches('\n').count() + 1;
        (first, self.line - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::super::cut::tests::{Fake, input};
    use super::super::types::Cut;
    use super::*;
    use serde_json::json;

    fn summary() -> ReleaseSummary {
        ReleaseSummary {
            summary: vec![Bullet {
                text: "The team planned the onboarding checklist [#0].".into(),
                citations: vec![0, 2],
            }],
            decisions: vec![],
            action_items: vec![Bullet {
                text: "John drafts the release notes.".into(),
                citations: vec![2],
            }],
        }
    }

    #[test]
    fn the_attachment_copies_retained_segments_and_marks_omissions() {
        let p = render(&input(), &[0, 2, 3], &summary()).unwrap();
        let lines: Vec<&str> = p.attachment.lines().collect();
        assert_eq!(lines[0], "# Notes: Weekly sync");
        // 2026-11-26T03:00:00Z is still the 25th in New York.
        assert_eq!(lines[2], "Meeting: 2026-11-25 22:00 EST (America/New_York)");
        assert_eq!(
            lines[4],
            "**00:00:00 | 10:00:00 PM EST · S1** Ship the onboarding checklist by Friday."
        );
        assert_eq!(lines[6], "*[… 1 segments omitted …]*");
        assert_eq!(
            lines[8],
            "**00:02:10 | 10:02:10 PM EST · S1** [John] will draft the release notes."
        );
        assert_eq!(
            lines[10],
            "**00:03:15 | 10:03:15 PM EST · S2** Thanks all, talk next week."
        );
        assert_eq!(lines[12], "*[… remainder omitted …]*");
        assert_eq!(lines.len(), 13);
        assert_eq!(
            p.index_map.attachment_lines,
            vec![(4, 4, 0), (8, 8, 2), (10, 10, 3)]
        );
        assert_eq!(
            p.index_map.bullets,
            vec![
                (Section::Summary, 0, vec![0, 2]),
                (Section::ActionItems, 0, vec![2])
            ]
        );
        assert_eq!(
            p.body,
            "Summary\n- The team planned the onboarding checklist .\n\n\
             Action items\n- John drafts the release notes.\n"
        );
    }

    #[tokio::test]
    async fn omitted_text_appears_nowhere_in_the_body_or_attachment() {
        let input = input();
        let cut = super::super::cut::run_with(
            &Fake::new(&[json!({"end_index":3,"omit_ranges":[[1,1]]})]),
            "test",
            &input,
        )
        .await
        .unwrap();
        let retained = cut.retained(input.segments.len());
        let summary = super::super::summary::run_with(
            &Fake::new(&[json!({
                "summary":[{"text":"Checklist ships Friday.","citations":[0]}],
                "decisions":[],
                "action_items":[{"text":"John drafts notes.","citations":[2]}]
            })]),
            "test",
            &input,
            &retained,
        )
        .await
        .unwrap();
        let p = render(&input, &retained, &summary).unwrap();
        assert!(
            p.attachment
                .contains("Ship the onboarding checklist by Friday.")
        );
        assert!(p.body.contains("- Checklist ships Friday."));
        for omitted in ["PRIVATE-TANGENT", "landlord", "AFTER-CALL", "salary"] {
            assert!(!p.body.contains(omitted));
            assert!(!p.attachment.contains(omitted));
            assert!(!p.subject.contains(omitted));
        }
    }

    #[test]
    fn a_reviewer_retained_set_can_bring_back_cut_text() {
        let cut = Cut {
            end_index: 3,
            omit_ranges: vec![[1, 1]],
            reviewer_retained: Some(vec![0, 1, 4]),
        };
        let p = render(&input(), &cut.retained(5), &summary()).unwrap();
        assert!(
            p.attachment
                .contains("PRIVATE-TANGENT my landlord is raising the rent")
        );
        assert!(
            p.attachment
                .contains("AFTER-CALL anyway about your salary review")
        );
        assert!(p.attachment.contains("*[… 2 segments omitted …]*"));
        assert!(!p.attachment.contains("remainder omitted"));
        assert_eq!(
            p.index_map
                .attachment_lines
                .iter()
                .map(|l| l.2)
                .collect::<Vec<_>>(),
            vec![0, 1, 4]
        );
    }

    #[test]
    fn only_this_meetings_segments_are_rendered() {
        let p = render(&input(), &[0, 99], &summary()).unwrap();
        assert_eq!(
            p.index_map
                .attachment_lines
                .iter()
                .map(|l| l.2)
                .collect::<Vec<_>>(),
            vec![0]
        );
        assert!(!p.attachment.contains("[#99]"));
        assert!(p.attachment.ends_with("*[… remainder omitted …]*\n"));
    }

    #[test]
    fn the_subject_and_slug_follow_d9_for_a_title_with_newlines() {
        let mut i = input();
        i.event_title = "Q3 plan\r\nBob & Ada\t— review".into();
        let p = render(&i, &[0], &summary()).unwrap();
        assert_eq!(p.subject, "Notes: Q3 plan  Bob & Ada — review (2026-11-25)");
        assert!(!p.subject.contains('\n') && !p.subject.contains('\r'));
        assert_eq!(p.attachment_name, "2026-11-25-q3-plan-bob-ada-review.md");
        assert!(
            p.attachment
                .starts_with("# Notes: Q3 plan  Bob & Ada — review\n")
        );

        i.event_title = "x".repeat(400);
        let p = render(&i, &[0], &summary()).unwrap();
        assert_eq!(p.subject.chars().count(), 150);
        assert!(p.subject.ends_with(" (2026-11-25)"));
        assert_eq!(
            p.attachment_name,
            format!("2026-11-25-{}.md", "x".repeat(60))
        );

        i.event_title = " \n—!! ".into();
        let p = render(&i, &[0], &summary()).unwrap();
        assert_eq!(p.subject, "Notes: —!! (2026-11-25)");
        assert_eq!(p.attachment_name, "2026-11-25-meeting.md");
    }

    #[test]
    fn the_date_uses_the_meetings_zone() {
        let mut i = input();
        i.tz = "Asia/Tokyo".into();
        let p = render(&i, &[0], &summary()).unwrap();
        assert!(p.subject.ends_with("(2026-11-26)"));
        assert!(p.attachment_name.starts_with("2026-11-26-"));
    }

    #[test]
    fn a_body_over_64_kb_is_held() {
        let big = ReleaseSummary {
            summary: (0..80)
                .map(|_| Bullet {
                    text: "y".repeat(900),
                    citations: vec![0],
                })
                .collect(),
            ..ReleaseSummary::default()
        };
        assert_eq!(
            render(&input(), &[0], &big),
            Err(PrepareError::BodyTooLarge)
        );
    }
}
