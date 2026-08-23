//! Which LLM engine a meeting gets, and what happens with none — #67, #68.
//!
//! The order is a policy, pinned here: an Anthropic API key always wins
//! (today's behavior, untouched); with no key, the `claude` CLI serves — but
//! only when the user explicitly enabled it *and* acknowledged that the
//! transcript leaves the machine (KEY-04 applies to this path exactly as it
//! applies to a key: the words travel either way, only the bill differs).
//! With neither, enrichment degrades to a local fallback title and nothing
//! leaves the device.

use fotw_secrets::{InMemoryKeyStore, KeyStore, Provider, SecretKey, SecretString};
use fotw_stt::Source;
use fotw_stt::transcript::{TimestampSource, TranscriptSegment};
use fotwd::engine::{Engine, SummarizeSettings, fallback_title, resolve_engine};

fn db() -> fotw_store::Db {
    fotw_store::Db::open_in_memory(&key32()).unwrap()
}

fn key32() -> fotw_store::DbKey {
    fotw_store::DbKey::from_bytes([7u8; 32])
}

fn store_with_anthropic_key() -> InMemoryKeyStore {
    let store = InMemoryKeyStore::new();
    store
        .set(
            SecretKey::ApiKey(Provider::Anthropic),
            &SecretString::new("sk-ant-test"),
        )
        .unwrap();
    store
}

/// Settings rows the tests write directly, as `fotwd engine` would.
fn write_settings(db: &mut fotw_store::Db, settings: &SummarizeSettings) {
    db.put_setting("summarize", &serde_json::to_string(settings).unwrap())
        .unwrap();
}

fn cli_settings(binary: &str, acknowledged: bool) -> SummarizeSettings {
    SummarizeSettings {
        cli_enabled: true,
        acknowledged_egress: acknowledged,
        binary: binary.to_owned(),
        ..Default::default()
    }
}

// -------------------------------------------------------------- resolution

#[test]
fn no_key_and_no_cli_means_no_engine() {
    let db = db();
    assert!(resolve_engine(&InMemoryKeyStore::new(), &db).is_none());
}

#[test]
fn an_api_key_always_wins() {
    let mut db = db();
    // Even with the CLI fully enabled, the key is the explicit configuration
    // and keeps today's behavior byte-for-byte.
    write_settings(&mut db, &cli_settings("/bin/echo", true));

    match resolve_engine(&store_with_anthropic_key(), &db) {
        Some(Engine::Anthropic { .. }) => {}
        other => panic!("expected the API engine, got {other:?}"),
    }
}

#[test]
fn the_cli_serves_when_enabled_and_acknowledged_and_present() {
    let mut db = db();
    // /bin/echo exists on every machine this test runs on; the resolver only
    // asks whether the configured binary is executable, not whether it is
    // really the claude CLI — that is the invocation's job to discover.
    write_settings(&mut db, &cli_settings("/bin/echo", true));

    match resolve_engine(&InMemoryKeyStore::new(), &db) {
        Some(Engine::ClaudeCli { binary }) => {
            assert_eq!(binary, std::path::PathBuf::from("/bin/echo"));
        }
        other => panic!("expected the CLI engine, got {other:?}"),
    }
}

/// Enablement without the egress acknowledgement is not enablement. A
/// transcript silently flowing into whatever binary is on PATH is the kind
/// of surprise this project exists to not do.
#[test]
fn an_unacknowledged_cli_is_no_engine() {
    let mut db = db();
    write_settings(&mut db, &cli_settings("/bin/echo", false));
    assert!(resolve_engine(&InMemoryKeyStore::new(), &db).is_none());
}

#[test]
fn a_missing_binary_is_no_engine_rather_than_a_later_surprise() {
    let mut db = db();
    write_settings(&mut db, &cli_settings("/no/such/binary/anywhere", true));
    assert!(resolve_engine(&InMemoryKeyStore::new(), &db).is_none());
}

// --------------------------------------------------------- fallback titles

fn seg(start_ms: u64, text: &str) -> TranscriptSegment {
    TranscriptSegment {
        id: format!("{start_ms}"),
        session_id: "s".to_owned(),
        source: Source::System,
        speaker: None,
        text: text.to_owned(),
        start_ms,
        end_ms: start_ms + 1_000,
        words: Vec::new(),
        confidence: None,
        language: None,
        is_final: true,
        revision: 0,
        provider: "deepgram".to_owned(),
        model: "nova-3".to_owned(),
        timestamp_source: TimestampSource::Provider,
    }
}

/// With no engine, the first substantive utterance beats an epoch number.
#[test]
fn the_fallback_title_is_the_first_substantive_utterance() {
    let segs = vec![
        seg(0, "Um."),
        seg(
            1_000,
            "Okay so the interconnect bandwidth question from last week",
        ),
        seg(9_000, "right, exactly"),
    ];
    let title = fallback_title(&segs).expect("there is speech to draw from");
    assert!(
        title.starts_with("Okay so the interconnect"),
        "picked the wrong utterance: {title}"
    );
    assert!(title.len() <= 64, "a title is not a paragraph: {title:?}");
}

/// Short grunts do not become titles.
#[test]
fn noise_alone_yields_no_title() {
    assert!(fallback_title(&[seg(0, "Um."), seg(1, "yeah")]).is_none());
    assert!(fallback_title(&[]).is_none());
}

/// A long utterance is cut at a word boundary, not mid-word.
#[test]
fn a_long_utterance_is_trimmed_at_a_word_boundary() {
    let long = "the quarterly numbers look strong and the interconnect bandwidth \
                question from last week is now resolved so we can move forward";
    let title = fallback_title(&[seg(0, long)]).unwrap();
    assert!(title.len() <= 64);
    assert!(
        long.starts_with(title.trim_end_matches('…')),
        "the trim invented words: {title:?}"
    );
    assert!(!title.contains("bandwi…"), "mid-word cut: {title:?}");
}
