//! The dashboard's engine settings, backed by the real library and keychain.
//!
//! [`fotw_web::SummarizeControl`] over its own [`Db`] plus the OS keystore —
//! the [`GithubExporter`](crate::github::GithubExporter) precedent, for the
//! same reason: the UI's `StoreSource` mutex must never wait on a keychain
//! prompt or a filesystem probe.
//!
//! # Why this is not just a settings CRUD
//!
//! The interesting half is [`EngineControl::status`], which answers "what
//! would this daemon do right now" by running the daemon's own resolver. Both
//! `fotwd engine` and this call [`resolve_binary`], so the terminal and the
//! dashboard cannot disagree — which they did, and that disagreement was
//! mechanism two of #74: the status arm re-ran the resolver *in the user's
//! shell*, where `~/.local/bin` is on `$PATH`, and reported an engine the
//! daemon could not see.

use std::sync::{Mutex, MutexGuard};

use fotw_secrets::{KeyStore, Provider, SecretKey};
use fotw_store::Db;
use fotw_web::{SummarizeControl, SummarizeError, SummarizeSettingsDoc, SummarizeStatus};

use crate::engine::{CliKind, SETTINGS_KEY, SummarizeSettings, resolve_binary};

/// Reads and writes the engine choice, and reports what it resolves to.
pub struct EngineControl {
    db: Mutex<Db>,
    store: &'static dyn KeyStore,
}

impl std::fmt::Debug for EngineControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineControl(<redacted>)")
    }
}

impl EngineControl {
    /// Over an open library and the process keystore.
    #[must_use]
    pub fn new(db: Db, store: &'static dyn KeyStore) -> Self {
        Self {
            db: Mutex::new(db),
            store,
        }
    }

    fn lock_db(&self) -> MutexGuard<'_, Db> {
        // A panic inside a query leaves SQLite consistent — rusqlite's guard
        // rolls the transaction back — so the poison flag carries no
        // information, and honouring it would take the settings pane down for
        // the rest of the session.
        self.db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SummarizeControl for EngineControl {
    fn settings(&self) -> SummarizeSettingsDoc {
        to_doc(&SummarizeSettings::read(&self.lock_db()))
    }

    fn set_settings(
        &self,
        settings: SummarizeSettingsDoc,
    ) -> Result<SummarizeSettingsDoc, SummarizeError> {
        let mut db = self.lock_db();
        let previous = SummarizeSettings::read(&db);

        let kind = CliKind::from(settings.cli_kind);
        let stored = SummarizeSettings {
            cli_enabled: settings.cli_enabled,
            // The handler already refused enablement without this (KEY-04);
            // storing it verbatim keeps the resolver's own second check —
            // which is the one enrichment actually consults — meaningful.
            acknowledged_egress: settings.acknowledged_egress,
            cli_kind: kind,
            // An empty binary means "whatever this engine installs as". Keep
            // the previous spelling when the kind is unchanged, so a user who
            // edits the checkbox does not silently lose a path they typed.
            binary: if !settings.binary.is_empty() {
                settings.binary.clone()
            } else if previous.cli_kind == kind && !previous.binary.is_empty() {
                previous.binary.clone()
            } else {
                kind.default_binary()
            },
        };

        let json = serde_json::to_string(&stored)
            .map_err(|e| SummarizeError::Failed(format!("could not encode the settings: {e}")))?;
        db.put_setting(SETTINGS_KEY, &json)
            .map_err(|e| SummarizeError::Failed(format!("could not store the settings: {e}")))?;
        Ok(to_doc(&stored))
    }

    fn status(&self) -> SummarizeStatus {
        let settings = SummarizeSettings::read(&self.lock_db());
        // Presence, never the value. KEY-01 keeps the key in the keychain, and
        // "is one configured" is all a settings form needs to render.
        let api_key_present = self
            .store
            .contains(SecretKey::ApiKey(Provider::Anthropic))
            .unwrap_or(false);

        let resolved = (settings.cli_enabled && settings.acknowledged_egress)
            .then(|| resolve_binary(&settings.binary))
            .flatten();

        let engine = if api_key_present {
            // An explicit key always wins, whatever the CLI settings say —
            // saying otherwise here would describe an engine that never runs.
            "anthropic"
        } else if resolved.is_some() {
            settings.cli_kind.subcommand()
        } else {
            "none"
        };

        SummarizeStatus {
            engine: engine.to_owned(),
            binary_resolves: resolved.is_some(),
            configured_binary: settings.binary.clone(),
            resolved_binary: resolved.map(|p| p.to_string_lossy().into_owned()),
            api_key_present,
            disclosures: SummarizeStatus::all_disclosures(),
        }
    }
}

fn to_doc(settings: &SummarizeSettings) -> SummarizeSettingsDoc {
    SummarizeSettingsDoc {
        cli_enabled: settings.cli_enabled,
        acknowledged_egress: settings.acknowledged_egress,
        cli_kind: settings.cli_kind.into(),
        binary: settings.binary.clone(),
    }
}
