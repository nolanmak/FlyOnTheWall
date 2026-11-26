//! The release review eval corpus (#330, §7) keeps the shape the runner and
//! the acceptance numbers rely on. Offline: reads JSON, calls no engine.

use std::collections::BTreeMap;
use std::path::PathBuf;

use fotwd::release::types::Category;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Meeting {
    id: String,
    kind: String,
    event_title: String,
    tz: String,
    started_at_ms: i64,
    scheduled_minutes: u64,
    recipients: Vec<Recipient>,
    segments: Vec<Segment>,
    planted: Vec<Planted>,
    #[allow(dead_code)]
    notes: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipient {
    address: String,
    display_name: Option<String>,
    is_self: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Segment {
    offset_s: u64,
    speaker: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Planted {
    category: Category,
    indices: Vec<usize>,
    quote: String,
    #[allow(dead_code)]
    note: String,
}

fn corpus() -> Vec<(String, Meeting)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/release-eval/corpus");
    let mut out: Vec<_> = std::fs::read_dir(&dir)
        .expect("corpus dir")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
            let raw = std::fs::read_to_string(&p).unwrap();
            let m: Meeting = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{stem}: {e}"));
            (stem, m)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn the_corpus_meets_the_section_7_shape() {
    let meetings = corpus();
    let planted = meetings.iter().filter(|(_, m)| m.kind == "planted").count();
    let clean = meetings.iter().filter(|(_, m)| m.kind == "clean").count();
    assert!(planted >= 30, "{planted} planted meetings");
    assert!(clean >= 10, "{clean} clean meetings");

    let mut per_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut tail = false;
    let mut overrun = false;
    for (stem, m) in &meetings {
        assert_eq!(&m.id, stem);
        assert!(m.kind == "planted" || m.kind == "clean", "{stem}");
        assert!(!m.event_title.is_empty() && !m.tz.is_empty() && m.started_at_ms > 0);
        assert_eq!(
            m.recipients.iter().filter(|r| r.is_self).count(),
            1,
            "{stem}"
        );
        for r in &m.recipients {
            let domain = r.address.rsplit('@').next().unwrap();
            assert!(domain == "example.com" || domain == "example.org", "{stem}");
        }
        assert!(
            m.segments.windows(2).all(|w| w[0].offset_s < w[1].offset_s),
            "{stem}"
        );
        assert!(
            m.segments
                .iter()
                .all(|s| !s.speaker.is_empty() && !s.text.is_empty())
        );
        assert_eq!(m.kind == "clean", m.planted.is_empty(), "{stem}");
        for p in &m.planted {
            assert!(!p.indices.is_empty() && p.indices.iter().all(|i| *i < m.segments.len()));
            assert!(p.quote.len() >= 20, "{stem}: quote too short");
            assert!(
                m.segments[p.indices[0]].text.contains(&p.quote),
                "{stem}: quote"
            );
            let name = serde_json::to_value(p.category).unwrap();
            *per_category
                .entry(name.as_str().unwrap().to_owned())
                .or_default() += 1;
            if p.category == Category::AboutRecipient {
                let text: String = p
                    .indices
                    .iter()
                    .map(|i| m.segments[*i].text.as_str())
                    .collect();
                assert!(
                    m.recipients
                        .iter()
                        .filter_map(|r| r.display_name.as_deref())
                        .any(|n| { text.contains(n.split_whitespace().next().unwrap()) }),
                    "{stem}: about_recipient must name a recipient"
                );
            }
            if p.category == Category::OffMeeting
                && *p.indices.last().unwrap() == m.segments.len() - 1
                && p.indices.len() >= 4
            {
                tail = true;
            }
        }
        let last = m.segments.last().unwrap().offset_s;
        if m.kind == "clean" && last >= m.scheduled_minutes * 60 + 30 * 60 {
            overrun = true;
        }
    }
    assert_eq!(per_category.len(), 6, "{per_category:?}");
    assert!(per_category.values().all(|n| *n >= 5), "{per_category:?}");
    assert!(tail, "an after-meeting tail");
    assert!(overrun, "a clean meeting running 30+ minutes over, no tail");
}

#[test]
fn offensive_passages_use_the_placeholder_not_real_language() {
    let meetings = corpus();
    let slurs = meetings
        .iter()
        .flat_map(|(_, m)| &m.segments)
        .filter(|s| s.text.contains("[SLUR]"))
        .count();
    assert!(slurs >= 3, "{slurs}");
}
