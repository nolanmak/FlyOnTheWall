//! The real [`Preparer`]: the configured summary engine running the release
//! cut, summary, render and review (#326 integration of #329 and #330).
//!
//! The engine is resolved on every call rather than once at start-up, so a
//! user who configures or changes the engine after the daemon started gets
//! release without a restart, and the engine id recorded with each decision
//! (D11, D19) is the one that actually prepared the row.

use std::path::PathBuf;

use fotw_secrets::KeyStore;
use fotw_summarize::Preset;

use crate::engine::Engine;
use crate::release::types::{
    BoxFuture, Cut, Finding, Payload, PrepareError, PrepareInput, Preparer, ReleaseSummary,
};

/// Resolves the configured engine per call; see the module docs.
pub struct EnginePreparer {
    store: &'static dyn KeyStore,
    root: PathBuf,
}

impl EnginePreparer {
    /// A preparer over the library at `root` and the daemon's key store.
    #[must_use]
    pub fn new(store: &'static dyn KeyStore, root: PathBuf) -> Self {
        Self { store, root }
    }

    /// The engine configured right now. Policy refusal is the worker's
    /// pre-pass check (D18); here "no engine" means the call cannot run.
    fn engine(&self) -> Result<Engine, PrepareError> {
        let db = crate::open_library(&self.root).map_err(|_| PrepareError::ModelFailed)?;
        crate::engine::resolve_engine(self.store, &db).ok_or(PrepareError::PolicyRefused)
    }
}

/// Engine kind plus model name (D11): the unit the auto unlock counts in.
fn engine_id_for(engine: &Engine) -> String {
    let kind = match engine {
        Engine::Anthropic { .. } => "anthropic-api",
        Engine::ClaudeCli { .. } => "claude-cli",
        Engine::Codex { .. } => "codex-cli",
    };
    let model = Preset::Balanced.prose_model().unwrap_or("default");
    format!("{kind}:{model}")
}

impl Preparer for EnginePreparer {
    fn cut<'a>(&'a self, input: &'a PrepareInput) -> BoxFuture<'a, Result<Cut, PrepareError>> {
        Box::pin(async move { crate::release::cut::run(&self.engine()?, input).await })
    }

    fn summarize<'a>(
        &'a self,
        input: &'a PrepareInput,
        retained: &'a [usize],
    ) -> BoxFuture<'a, Result<ReleaseSummary, PrepareError>> {
        Box::pin(
            async move { crate::release::summary::run(&self.engine()?, input, retained).await },
        )
    }

    fn render(
        &self,
        input: &PrepareInput,
        retained: &[usize],
        summary: &ReleaseSummary,
    ) -> Result<Payload, PrepareError> {
        crate::release::render::render(input, retained, summary)
    }

    fn review<'a>(
        &'a self,
        input: &'a PrepareInput,
        retained: &'a [usize],
        payload: &'a Payload,
    ) -> BoxFuture<'a, Result<Vec<Finding>, PrepareError>> {
        Box::pin(async move {
            crate::release::review::run(&self.engine()?, input, retained, payload).await
        })
    }

    fn engine_id(&self) -> String {
        self.engine()
            .map_or_else(|_| "none".to_owned(), |engine| engine_id_for(&engine))
    }
}
