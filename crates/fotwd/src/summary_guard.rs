//! Fence model dispatch, awaited results and publication by settings revision.
use fotw_store::{Db, SettingVersion};
use fotw_summarize::transport::BoxFuture;
use fotw_summarize::{Capabilities, LlmAdapter, LlmRequest, LlmResponse, SummarizeError};

use crate::engine::{Engine, SETTINGS_KEY, SummarizeSettings};
use fotw_web::SummaryPolicy;

pub(crate) fn changed() -> SummarizeError {
    SummarizeError::Transport("Summary settings changed or became unavailable; no result was saved. Retry with the selected destination.".into())
}

pub(crate) struct PolicyStamp(SettingVersion);

impl PolicyStamp {
    pub(crate) fn capture(db: &Db, engine: &Engine) -> Result<Self, SummarizeError> {
        let version = match engine.settings_version() {
            Some(version) => version.clone(),
            None => db.setting_version(SETTINGS_KEY).map_err(|_| changed())?,
        };
        let stamp = Self(version);
        stamp.check(db)?;
        let settings = SummarizeSettings::from_snapshot(db, stamp.0.value());
        let permitted = match settings.policy {
            // Injected engines support the same pre-policy test seam. Actual
            // resolvers additionally bind their settings generation above.
            None => true,
            Some(SummaryPolicy::Anthropic) => {
                settings.acknowledged_egress && matches!(engine, Engine::Anthropic { .. })
            }
            Some(SummaryPolicy::ClaudeCli) => {
                settings.acknowledged_egress && matches!(engine, Engine::ClaudeCli { .. })
            }
            Some(SummaryPolicy::CodexCli) => {
                settings.acknowledged_egress && matches!(engine, Engine::Codex { .. })
            }
            Some(SummaryPolicy::Off | SummaryPolicy::LocalOnly) => false,
        };
        if !permitted {
            return Err(changed());
        }
        Ok(stamp)
    }

    pub(crate) fn check(&self, db: &Db) -> Result<(), SummarizeError> {
        if self.0.is_current(db).map_err(|_| changed())? {
            Ok(())
        } else {
            Err(changed())
        }
    }

    pub(crate) fn version(&self) -> &SettingVersion {
        &self.0
    }
}

pub(crate) struct GuardedAdapter<'a> {
    inner: &'a dyn LlmAdapter,
    check: &'a (dyn Fn() -> Result<(), SummarizeError> + Send + Sync),
}

impl<'a> GuardedAdapter<'a> {
    pub(crate) fn new(
        inner: &'a dyn LlmAdapter,
        check: &'a (dyn Fn() -> Result<(), SummarizeError> + Send + Sync),
    ) -> Self {
        Self { inner, check }
    }
}

impl LlmAdapter for GuardedAdapter<'_> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn complete<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> BoxFuture<'a, Result<LlmResponse, SummarizeError>> {
        Box::pin(async move {
            (self.check)()?;
            let work = self.inner.complete(request);
            tokio::pin!(work);
            let mut timer = tokio::time::interval(std::time::Duration::from_millis(100));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = timer.tick() => (self.check)()?,
                    result = &mut work => {
                        (self.check)()?;
                        return result;
                    }
                }
            }
        })
    }
}
