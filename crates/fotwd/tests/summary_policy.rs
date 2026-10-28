//! Explicit local/off choices must beat legacy hosted credentials (#240).
use std::sync::atomic::{AtomicUsize, Ordering};

use fotw_secrets::{InMemoryKeyStore, KeyStore, Provider, SecretKey, SecretString, SecretsError};
use fotwd::engine::{Engine, resolve_engine};

struct ObservedStore {
    inner: InMemoryKeyStore,
    reads: AtomicUsize,
}

impl ObservedStore {
    fn configured() -> Self {
        let inner = InMemoryKeyStore::new();
        inner
            .set(
                SecretKey::ApiKey(Provider::Anthropic),
                &SecretString::new("sk-ant-test"),
            )
            .unwrap();
        Self {
            inner,
            reads: AtomicUsize::new(0),
        }
    }
}

impl KeyStore for ObservedStore {
    fn set(&self, key: SecretKey, value: &SecretString) -> Result<(), SecretsError> {
        self.inner.set(key, value)
    }
    fn get(&self, key: SecretKey) -> Result<SecretString, SecretsError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key)
    }
    fn delete(&self, key: SecretKey) -> Result<(), SecretsError> {
        self.inner.delete(key)
    }
    fn contains(&self, key: SecretKey) -> Result<bool, SecretsError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.contains(key)
    }
}

fn configured_db(policy: &str) -> fotw_store::Db {
    let mut db = fotw_store::Db::open_in_memory(&fotw_store::DbKey::from_bytes([7; 32])).unwrap();
    db.put_setting(
        "summarize",
        &serde_json::json!({
            "policy": policy,
            "cli_enabled": true,
            "acknowledged_egress": true,
            "cli_kind": "codex",
            "binary": "/bin/echo"
        })
        .to_string(),
    )
    .unwrap();
    db
}

#[test]
fn explicit_off_never_reads_a_key_or_selects_a_hosted_engine() {
    let store = ObservedStore::configured();
    let db = configured_db("off");
    assert!(
        resolve_engine(&store, &db).is_none(),
        "Off was overridden by a stored hosted key"
    );
    assert_eq!(store.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn local_only_without_a_ready_local_runtime_never_falls_back_to_hosted() {
    let store = ObservedStore::configured();
    let db = configured_db("local_only");
    assert!(
        resolve_engine(&store, &db).is_none(),
        "local setup failure must not select hosted inference"
    );
    assert_eq!(store.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn invalid_explicit_policy_or_corrupt_settings_cannot_restore_legacy_egress() {
    for settings in [
        serde_json::json!({"policy": "future_unknown_destination"}).to_string(),
        "{broken".to_owned(),
    ] {
        let store = ObservedStore::configured();
        let mut db = configured_db("off");
        db.put_setting("summarize", &settings).unwrap();
        assert!(resolve_engine(&store, &db).is_none());
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn legacy_libraries_keep_their_previous_explicit_api_key_behavior() {
    let store = ObservedStore::configured();
    let db = fotw_store::Db::open_in_memory(&fotw_store::DbKey::from_bytes([7; 32])).unwrap();
    // Explicitly model an upgraded legacy library, not a new library.
    db.conn()
        .execute("DELETE FROM settings WHERE key='summarize'", [])
        .unwrap();
    db.conn()
        .execute(
            "UPDATE app_meta SET value='legacy' WHERE key='summary_destination_bootstrap'",
            [],
        )
        .unwrap();
    assert!(matches!(
        resolve_engine(&store, &db),
        Some(Engine::Anthropic { .. })
    ));
}

#[test]
fn local_and_off_status_do_not_read_hosted_keys_and_old_clients_cannot_clear_the_choice() {
    use fotw_web::{SummarizeControl, SummarizeSettingsDoc, SummaryPolicy};
    for (policy, expected, engine) in [
        ("off", SummaryPolicy::Off, "off"),
        (
            "local_only",
            SummaryPolicy::LocalOnly,
            "local-setup-required",
        ),
    ] {
        let store = Box::leak(Box::new(ObservedStore::configured()));
        let controller = fotwd::engine_control::EngineControl::new(configured_db(policy), store);
        let before = controller.status();
        assert_eq!(before.engine, engine);
        assert!(!before.api_key_checked);
        let saved = controller
            .set_settings(SummarizeSettingsDoc {
                cli_enabled: true,
                acknowledged_egress: true,
                binary: "/bin/echo".into(),
                ..SummarizeSettingsDoc::default()
            })
            .unwrap();
        assert_eq!(
            saved.policy,
            Some(expected),
            "missing policy in an old client must not restore legacy key precedence"
        );
        assert!(!saved.cli_enabled);
        assert!(!saved.acknowledged_egress);
        assert_eq!(controller.status().engine, engine);
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn an_explicit_acknowledged_cli_destination_ignores_a_stored_api_key() {
    use fotw_web::{CliEngine, SummarizeControl, SummarizeSettingsDoc, SummaryPolicy};
    let store = Box::leak(Box::new(ObservedStore::configured()));
    let controller = fotwd::engine_control::EngineControl::new(configured_db("local_only"), store);
    let saved = controller
        .set_settings(SummarizeSettingsDoc {
            policy: Some(SummaryPolicy::CodexCli),
            acknowledged_egress: true,
            cli_kind: CliEngine::Claude,
            binary: "/bin/echo".into(),
            ..SummarizeSettingsDoc::default()
        })
        .unwrap();
    assert_eq!(saved.cli_kind, CliEngine::Codex);
    assert!(saved.cli_enabled);
    assert_eq!(controller.status().engine, "codex-cli");
    assert_eq!(store.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn choosing_off_or_local_does_not_erase_an_inactive_custom_cli_path() {
    use fotw_web::{CliEngine, SummarizeControl, SummarizeSettingsDoc, SummaryPolicy};
    for policy in [SummaryPolicy::Off, SummaryPolicy::LocalOnly] {
        let store = Box::leak(Box::new(ObservedStore::configured()));
        let controller = fotwd::engine_control::EngineControl::new(configured_db("off"), store);
        controller
            .set_settings(SummarizeSettingsDoc {
                policy: Some(SummaryPolicy::CodexCli),
                acknowledged_egress: true,
                binary: "/owned/fixture-codex".into(),
                ..SummarizeSettingsDoc::default()
            })
            .unwrap();
        let saved = controller
            .set_settings(SummarizeSettingsDoc {
                policy: Some(policy),
                ..SummarizeSettingsDoc::default()
            })
            .unwrap();
        assert_eq!(saved.cli_kind, CliEngine::Codex);
        assert_eq!(saved.binary, "/owned/fixture-codex");
        assert_eq!(saved.policy, Some(policy));
        assert!(!saved.acknowledged_egress);
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn removing_fresh_defaults_or_damaging_the_bootstrap_marker_cannot_restore_implicit_hosted_calls() {
    for provenance in [None, Some("explicit-off-v1"), Some("unknown-future-policy")] {
        let db = fotw_store::Db::open_in_memory(&fotw_store::DbKey::from_bytes([7; 32])).unwrap();
        db.conn()
            .execute("DELETE FROM settings WHERE key='summarize'", [])
            .unwrap();
        db.conn()
            .execute(
                "DELETE FROM app_meta WHERE key='summary_destination_bootstrap'",
                [],
            )
            .unwrap();
        if let Some(value) = provenance {
            db.conn().execute("INSERT INTO app_meta(key,value,updated_at) VALUES('summary_destination_bootstrap',?1,0)",[value]).unwrap();
        }
        let store = ObservedStore::configured();
        assert!(resolve_engine(&store, &db).is_none());
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    }
}
