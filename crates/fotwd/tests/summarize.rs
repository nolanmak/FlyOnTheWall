//! The glue between the library and the summarisation pipeline.
//!
//! `fotw-summarize` has 157 tests covering the pipeline itself. None of them
//! can reach *this*: loading segments back out of SQLite, rebuilding the
//! document, and versioning the result. That is where the seams are, and
//! every bug found in this project so far has lived in a seam rather than in
//! a component.

use std::sync::Arc;

use fotw_secrets::{InMemoryKeyStore, KeyStore, Provider, SecretKey, SecretString};
use fotw_store::{Db, DbKey};
use fotw_stt::{Source, TimestampSource, TranscriptSegment};
use fotw_summarize::template::{Template, TemplateSet};
use fotw_summarize::testing::MockTransport;
use fotwd::persist;
use fotwd::session::{LegBuffers, SessionOutcome};
use fotwd::summarize;

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fotwd-sum-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn db_at(dir: &std::path::Path) -> Db {
    Db::open(dir.join("db.sqlite3"), &DbKey::from_bytes([0x11; 32])).unwrap()
}

/// The shipped `general` template (SUM-08). Deliberately a real one rather
/// than an empty body: the daemon never runs without a template, so a test
/// that passed an empty string would be exercising a configuration that does
/// not exist in the product.
fn general() -> Template {
    TemplateSet::builtin().get("general").unwrap().clone()
}

fn keystore_with_anthropic() -> InMemoryKeyStore {
    let s = InMemoryKeyStore::new();
    s.set(
        SecretKey::ApiKey(Provider::Anthropic),
        &SecretString::new("test-key"),
    )
    .unwrap();
    s
}

fn seg(idx: u64, text: &str, start: u64) -> TranscriptSegment {
    TranscriptSegment {
        id: format!("seg-{idx}"),
        session_id: "s".into(),
        source: Source::System,
        speaker: Some("S0".into()),
        text: text.into(),
        start_ms: start,
        end_ms: start + 1_000,
        words: Vec::new(),
        confidence: Some(0.9),
        language: Some("en".into()),
        is_final: true,
        revision: 0,
        provider: "deepgram".into(),
        model: "nova-3".into(),
        timestamp_source: TimestampSource::Provider,
    }
}

fn seeded_meeting(db: &mut Db, dir: &std::path::Path) -> String {
    let outcome = SessionOutcome {
        dir: dir.to_path_buf(),
        started_at_ms: 0,
        system_samples: 1_000,
        mic_samples: 0,
        system_buffers: LegBuffers {
            silent: 0,
            total: 10,
        },
        mic_buffers: None,
        dropped_samples: 0,
        segments: vec![
            seg(
                0,
                "The quarterly numbers came in above target this month.",
                0,
            ),
            seg(
                1,
                "Priya will follow up with the infra team by Friday.",
                1_000,
            ),
        ],
        stt_errors: Vec::new(),
    };
    persist::persist_session(db, &outcome, "Weekly sync").unwrap()
}

#[tokio::test]
async fn a_meeting_with_no_transcript_is_a_typed_error_not_a_crash() {
    let dir = tmpdir("notranscript");
    let mut db = db_at(&dir);
    let store = keystore_with_anthropic();

    let outcome = SessionOutcome {
        dir: dir.to_path_buf(),
        ..Default::default()
    };
    let id = persist::persist_session(&mut db, &outcome, "No transcript").unwrap();

    let err = summarize::summarize_meeting_with(
        &mut db,
        &store,
        &id,
        &general(),
        Arc::new(MockTransport::new()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, summarize::SummarizeRunError::NoTranscript(_)));
}

/// Recording without an LLM key configured must leave the transcript intact
/// and say so, not fail in a way that suggests the meeting was damaged.
#[tokio::test]
async fn a_missing_key_is_reported_without_touching_the_transcript() {
    let dir = tmpdir("nokey");
    let mut db = db_at(&dir);
    let id = seeded_meeting(&mut db, &dir);
    let empty_store = InMemoryKeyStore::new();

    let err = summarize::summarize_meeting_with(
        &mut db,
        &empty_store,
        &id,
        &general(),
        Arc::new(MockTransport::new()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, summarize::SummarizeRunError::NoKey));

    // The transcript is untouched and still queryable.
    let tid = persist::primary_transcript_id(&mut db, &id).unwrap();
    assert_eq!(db.meetings().transcript_text(&tid).unwrap().len(), 2);
    assert!(db.meetings().current_summary(&id).unwrap().is_none());
}

#[tokio::test]
async fn the_stored_transcript_reaches_the_provider_as_a_document() {
    let dir = tmpdir("roundtrip");
    let mut db = db_at(&dir);
    let id = seeded_meeting(&mut db, &dir);
    let store = keystore_with_anthropic();

    let mock = Arc::new(MockTransport::new().with_json(serde_json::json!({
        "content": [{"type": "text", "text": "Numbers were above target."}],
        "usage": {"input_tokens": 100, "output_tokens": 20},
        "stop_reason": "end_turn"
    })));

    // Only one response is queued, so Call B runs out of mock and fails at the
    // transport — still a hard failure after #75, which softened only a schema
    // violation. What matters here is that the text made it out of SQLite and
    // into a request body at all.
    let _ = summarize::summarize_meeting_with(&mut db, &store, &id, &general(), Arc::clone(&mock))
        .await;

    assert!(mock.call_count() > 0, "no request was made");
    let body = mock.request(0).body.to_string();
    assert!(
        body.contains("quarterly numbers"),
        "the stored transcript did not reach the request body"
    );
    assert!(
        body.contains("Priya"),
        "only the first segment reached the request body"
    );
    // The key must be in the header, and it must be Anthropic's header, not a
    // bearer token — a detail my own brief originally had wrong.
    let headers = mock.request(0).headers;
    assert!(
        headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-api-key")),
        "expected x-api-key, got {headers:?}"
    );
}

#[tokio::test]
async fn the_request_never_carries_both_citations_and_a_structured_format() {
    let dir = tmpdir("mutex");
    let mut db = db_at(&dir);
    let id = seeded_meeting(&mut db, &dir);
    let store = keystore_with_anthropic();

    let mock = Arc::new(MockTransport::new().with_json(serde_json::json!({
        "content": [{"type": "text", "text": "x"}],
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "stop_reason": "end_turn"
    })));
    let _ = summarize::summarize_meeting_with(&mut db, &store, &id, &general(), Arc::clone(&mock))
        .await;

    // The predicate is `enabled == true`, not the mere presence of the key:
    // Call B sends `citations: {enabled: false}` explicitly, so a substring
    // check would flag correct code. Getting this wrong is how a test ends up
    // failing on behaviour that is right.
    let mut checked = 0;
    for r in mock.requests() {
        let citations_on = r.body["messages"][0]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|b| b["citations"]["enabled"] == serde_json::json!(true));
        let format_set = !r.body["output_config"]["format"].is_null();
        assert!(
            !(citations_on && format_set),
            "spec 8.4: that combination is an HTTP 400. body: {}",
            r.body
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no requests were inspected — the test is vacuous"
    );
}

/// #75: a Call B answer the pipeline cannot parse must still leave a summary
/// row behind — the prose Call A already wrote, plus a note saying what is
/// missing from it. Before this, the meeting ended with no summary at all and
/// the reason went to a stderr nobody receives (#74).
#[tokio::test]
async fn an_unparseable_extraction_still_stores_a_summary_with_a_warning() {
    let dir = tmpdir("badextraction");
    let mut db = db_at(&dir);
    let id = seeded_meeting(&mut db, &dir);
    let store = keystore_with_anthropic();

    let mock = Arc::new(
        MockTransport::new()
            .with_json(serde_json::json!({
                "content": [{"type": "text", "text": "Numbers were above target."}],
                "usage": {"input_tokens": 100, "output_tokens": 20},
                "stop_reason": "end_turn"
            }))
            .with_json(serde_json::json!({
                "content": [{"type": "text", "text": "I'm afraid I can't help with that."}],
                "usage": {"input_tokens": 100, "output_tokens": 8},
                "stop_reason": "end_turn"
            })),
    );

    let outcome =
        summarize::summarize_meeting_with(&mut db, &store, &id, &general(), Arc::clone(&mock))
            .await
            .expect("a summary with no items beats no summary");

    let stored = db
        .meetings()
        .current_summary(&id)
        .unwrap()
        .expect("the summary row was not written");
    assert!(
        stored.body_md.contains("Numbers were above target"),
        "Call A's prose did not reach the store: {}",
        stored.body_md
    );
    assert!(
        stored.body_md.contains('>') && stored.body_md.to_lowercase().contains("action items"),
        "the stored markdown carries no admonition about the missing items: {}",
        stored.body_md
    );
    // Named, not merely non-empty: `LowGrounding` alone would satisfy that.
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.to_lowercase().contains("extraction")),
        "no warning named the extraction failure: {:?}",
        outcome.warnings
    );
}

/// #84: spec 8.6 sets a 0.7 citation-coverage threshold and `coverage::measure`
/// computes it on every run. Before this the number reached `SummaryOutcome`
/// and stopped there, so a summary citing none of its claims looked — in the
/// web pane, in the export, in `fotwd summarize`'s output — exactly like one
/// citing all of them.
#[tokio::test]
async fn a_weakly_grounded_summary_says_so_above_the_summary_itself() {
    let dir = tmpdir("lowgrounding");
    let mut db = db_at(&dir);
    let id = seeded_meeting(&mut db, &dir);
    let store = keystore_with_anthropic();

    // A substantive claim (spec 8.6: over 12 words) carrying no citation, so
    // coverage measures 0 of 1 and the threshold is missed by the whole range.
    let mock = Arc::new(
        MockTransport::new()
            .with_json(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": "The quarterly numbers came in above target and the infra \
                             migration is scheduled to finish before the beta ships.",
                    "citations": []
                }],
                "usage": {"input_tokens": 100, "output_tokens": 20},
                "stop_reason": "end_turn"
            }))
            .with_json(serde_json::json!({
                "content": [{"type": "text", "text": "{\"action_items\":[],\"decisions\":[],\
                    \"open_questions\":[],\"follow_ups\":[],\"topics\":[]}"}],
                "usage": {"input_tokens": 100, "output_tokens": 8},
                "stop_reason": "end_turn"
            })),
    );

    let outcome =
        summarize::summarize_meeting_with(&mut db, &store, &id, &general(), Arc::clone(&mock))
            .await
            .expect("a weakly grounded summary is still a summary");

    assert!(
        outcome.coverage < 0.7,
        "the fixture did not produce a weakly grounded summary: {}",
        outcome.coverage
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.to_lowercase().contains("grounding")),
        "the measured coverage never became words: {:?}",
        outcome.warnings
    );

    let stored = db
        .meetings()
        .current_summary(&id)
        .unwrap()
        .expect("the summary row was not written");
    let banner = stored
        .body_md
        .find("low transcript grounding")
        .expect("spec 8.6's banner is nowhere the user reads the summary");
    let prose = stored
        .body_md
        .find("quarterly numbers")
        .expect("Call A's prose did not reach the store");
    // A banner is above the thing it is about. It also has to survive being
    // *shared*, which is what the banner asks the user to think twice about --
    // and what gets shared is `body_md`, not the pane it renders in.
    assert!(
        banner < prose,
        "the grounding caveat sits below the summary it qualifies: {}",
        stored.body_md
    );
}
