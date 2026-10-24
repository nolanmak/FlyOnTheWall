//! Original-recording note windows, independent of display-relative speech time.
use crate::document::TranscriptDocument;
use serde_json::json;
use std::collections::BTreeSet;

/// Half-width of the transcript context around a typed block.
pub const WINDOW_MS: u64 = 90_000;

/// A note block whose first input was observed during capture.
#[derive(Debug, Clone)]
pub struct Anchor {
    /// Stable note-block identity.
    pub id: String,
    /// Position in the edited note.
    pub block_idx: usize,
    /// Current block text, not an instruction to the model.
    pub text: String,
    /// Milliseconds from capture start, never from first speech.
    pub typed_at_ms: u64,
}

/// Render notes with a per-block window and deduplicated transcript excerpts.
/// Only segments in the current map chunk are available as context. Boundary
/// turns retain their original times/text instead of fabricating word clipping.
#[must_use]
pub fn render(
    document: &TranscriptDocument,
    indices: &[usize],
    body: &str,
    anchors: &[Anchor],
    duration_ms: u64,
) -> Option<String> {
    if anchors.is_empty() {
        return (!body.trim().is_empty()).then(|| body.to_owned());
    }
    let mut used = BTreeSet::new();
    let windows:Vec<_>=anchors.iter().map(|a| {
        if a.typed_at_ms>duration_ms {
            return json!({"block_id":a.id,"block_idx":a.block_idx,"text":a.text,"typed_at_ms":a.typed_at_ms,
                "status":"outside_recording","segment_ids":[]});
        }
        let start=a.typed_at_ms.saturating_sub(WINDOW_MS);
        let end=a.typed_at_ms.saturating_add(WINDOW_MS).min(duration_ms);
        let matching:Vec<_>=indices.iter().copied().filter(|&i| document.segment(i)
            .is_some_and(|s|s.start_ms<=end && s.end_ms>=start)).collect();
        used.extend(matching.iter().copied());
        json!({"block_id":a.id,"block_idx":a.block_idx,"text":a.text,"typed_at_ms":a.typed_at_ms,
            "start_ms":start,"end_ms":end,"status":if matching.is_empty(){"no_context_in_chunk"}else{"available"},"segment_ids":matching})
    }).collect();
    let excerpts:Vec<_>=used.into_iter().filter_map(|i| document.segment(i)).map(|s| json!({
        "segment_id":s.index,"start_ms":s.start_ms,"end_ms":s.end_ms,"speaker":s.speaker_label(),"text":s.text
    })).collect();
    Some(json!({"raw_notes":body,"windows":windows,"excerpts":excerpts}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_stt::{Source, TimestampSource, TranscriptSegment};
    use serde_json::Value;
    fn document() -> TranscriptDocument {
        let segments: Vec<_> = [0, 89_000, 90_000, 180_000, 270_000, 270_001, 500_000]
            .into_iter()
            .enumerate()
            .map(|(i, start)| TranscriptSegment {
                id: format!("s{i}"),
                session_id: "fixture".into(),
                source: Source::Mic,
                speaker: None,
                text: format!("fixture-{start}"),
                start_ms: start,
                end_ms: start + 100,
                words: vec![],
                confidence: None,
                language: None,
                is_final: true,
                revision: 0,
                provider: "fixture".into(),
                model: "fixture".into(),
                timestamp_source: TimestampSource::Provider,
            })
            .collect();
        TranscriptDocument::from_segments(&segments)
    }
    fn anchor(id: &str, time: u64) -> Anchor {
        Anchor {
            id: id.into(),
            block_idx: 0,
            text: "pricing??".into(),
            typed_at_ms: time,
        }
    }
    #[test]
    fn windows_use_capture_time_clip_at_edges_and_share_overlapping_excerpts() {
        let doc = document();
        let text = render(
            &doc,
            &(0..doc.len()).collect::<Vec<_>>(),
            "pricing??",
            &[
                anchor("middle", 180_000),
                anchor("same", 180_000),
                anchor("start", 0),
                anchor("end", 500_000),
            ],
            500_000,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&text).expect("structured note windows");
        assert_eq!(value["windows"][0]["start_ms"], 90_000);
        assert_eq!(value["windows"][0]["end_ms"], 270_000);
        assert_eq!(value["windows"][0]["segment_ids"], json!([2, 3, 4]));
        assert_eq!(value["windows"][2]["start_ms"], 0);
        assert_eq!(value["windows"][3]["end_ms"], 500_000);
        let excerpts = value["excerpts"].as_array().unwrap();
        assert_eq!(excerpts.iter().filter(|s| s["segment_id"] == 3).count(), 1);
        assert!(!excerpts.iter().any(|s| s["segment_id"] == 5));
    }
    #[test]
    fn chunk_windows_cannot_include_other_chunks_or_out_of_recording_anchors() {
        let doc = document();
        let text = render(
            &doc,
            &[3],
            "raw notes",
            &[anchor("here", 180_000), anchor("invalid", 500_001)],
            500_000,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["windows"][0]["segment_ids"], json!([3]));
        assert_eq!(value["windows"][1]["segment_ids"], json!([]));
        assert_eq!(value["windows"][1]["status"], "outside_recording");
        assert_eq!(value["excerpts"].as_array().unwrap().len(), 1);
        assert!(text.contains("fixture-180000"));
        assert!(!text.contains("fixture-270000"));
    }
}
