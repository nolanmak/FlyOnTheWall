//! Fence dispatch, awaited results and publication by settings/input generations.
use fotw_store::{Db, SettingVersion};
use fotw_summarize::transport::BoxFuture;
use fotw_summarize::{Capabilities, LlmAdapter, LlmRequest, LlmResponse, SummarizeError};

use crate::engine::{Engine, SETTINGS_KEY, SummarizeSettings};
use fotw_web::SummaryPolicy;

pub(crate) fn changed() -> SummarizeError {
    SummarizeError::Transport("The meeting content or summary settings changed or became unavailable. That result was not saved. Retry with the current version.".into())
}

/// Why the summary policy will not let a model call run or land (#326 D18).
///
/// Existing callers see every variant as [`changed`]; attendee release maps
/// them separately: `Refused` pauses the worker (`engine_unavailable`),
/// `SettingsChanged` retries the call next pass, `InputsChanged` restarts the
/// meeting's preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyError {
    /// The policy refuses this engine (for example `Off` or `LocalOnly`).
    Refused,
    /// The summary settings changed, or could not be read, since the engine
    /// was resolved or the stamp was taken.
    SettingsChanged,
    /// The meeting's inputs changed, or their generation is unavailable.
    InputsChanged,
}

impl PolicyError {
    /// The stable code, for logs (D15).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Refused => "engine_unavailable",
            Self::SettingsChanged => "settings_changed",
            Self::InputsChanged => "inputs_changed",
        }
    }
}

impl From<PolicyError> for crate::release::types::PrepareError {
    fn from(e: PolicyError) -> Self {
        match e {
            PolicyError::Refused => Self::PolicyRefused,
            PolicyError::SettingsChanged => Self::SettingsChanged,
            PolicyError::InputsChanged => Self::InputsChanged,
        }
    }
}

/// Whether `settings` let `engine` run at all: the one policy rule shared by
/// [`PolicyStamp::capture`] and [`policy_allows`].
fn permitted(settings: &SummarizeSettings, engine: &Engine) -> bool {
    match settings.policy {
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
    }
}

/// The settings generation `engine` was resolved under, or the current one
/// for an injected engine.
fn settings_version(db: &Db, engine: &Engine) -> Result<SettingVersion, PolicyError> {
    match engine.settings_version() {
        Some(version) => Ok(version.clone()),
        None => db
            .setting_version(SETTINGS_KEY)
            .map_err(|_| PolicyError::SettingsChanged),
    }
}

/// The meeting-less policy check (#326 D18): the same rule as
/// [`PolicyStamp::capture`], with no meeting. Attendee release runs it when
/// release is enabled and at the start of every pass.
///
/// # Errors
///
/// [`PolicyError::SettingsChanged`] when the engine's settings generation is
/// no longer current; [`PolicyError::Refused`] when the policy refuses it.
pub(crate) fn policy_allows(db: &Db, engine: &Engine) -> Result<(), PolicyError> {
    let version = settings_version(db, engine)?;
    if !version
        .is_current(db)
        .map_err(|_| PolicyError::SettingsChanged)?
    {
        return Err(PolicyError::SettingsChanged);
    }
    let settings = SummarizeSettings::from_snapshot(db, version.value());
    if permitted(&settings, engine) {
        Ok(())
    } else {
        Err(PolicyError::Refused)
    }
}

pub(crate) struct PolicyStamp {
    /// The settings generation alone, to tell a settings change from an
    /// inputs change.
    settings: SettingVersion,
    /// Settings plus the meeting's input generation.
    full: SettingVersion,
}

impl PolicyStamp {
    pub(crate) fn capture(
        db: &Db,
        engine: &Engine,
        meeting_id: &str,
    ) -> Result<Self, SummarizeError> {
        Self::capture_typed(db, engine, meeting_id).map_err(|_| changed())
    }

    /// [`Self::capture`], saying which of the three failures it was.
    pub(crate) fn capture_typed(
        db: &Db,
        engine: &Engine,
        meeting_id: &str,
    ) -> Result<Self, PolicyError> {
        let settings = settings_version(db, engine)?;
        let full = settings
            .clone()
            .with_meeting_inputs(db, meeting_id)
            .map_err(|_| PolicyError::InputsChanged)?;
        let stamp = Self { settings, full };
        stamp.check_typed(db)?;
        let snapshot = SummarizeSettings::from_snapshot(db, stamp.full.value());
        if !permitted(&snapshot, engine) {
            return Err(PolicyError::Refused);
        }
        Ok(stamp)
    }

    pub(crate) fn check(&self, db: &Db) -> Result<(), SummarizeError> {
        self.check_typed(db).map_err(|_| changed())
    }

    /// [`Self::check`], saying whether the settings or the inputs moved.
    pub(crate) fn check_typed(&self, db: &Db) -> Result<(), PolicyError> {
        if !self
            .settings
            .is_current(db)
            .map_err(|_| PolicyError::SettingsChanged)?
        {
            return Err(PolicyError::SettingsChanged);
        }
        if self
            .full
            .is_current(db)
            .map_err(|_| PolicyError::InputsChanged)?
        {
            Ok(())
        } else {
            Err(PolicyError::InputsChanged)
        }
    }

    pub(crate) fn version(&self) -> &SettingVersion {
        &self.full
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::types::PrepareError;

    fn db() -> Db {
        Db::open_in_memory(&fotw_store::DbKey::from_bytes([7; 32])).unwrap()
    }

    fn claude(db: &Db) -> Engine {
        Engine::ClaudeCli {
            binary: "/does-not-exist".into(),
            settings_version: Some(db.setting_version(SETTINGS_KEY).unwrap()),
        }
    }

    const CLAUDE_OK: &str = r#"{"policy":"claude_cli","acknowledged_egress":true}"#;

    #[test]
    fn policy_allows_refuses_what_the_sharing_policy_refuses() {
        let mut db = db();
        db.put_setting(SETTINGS_KEY, r#"{"policy":"off"}"#).unwrap();
        assert_eq!(policy_allows(&db, &claude(&db)), Err(PolicyError::Refused));
        db.put_setting(SETTINGS_KEY, r#"{"policy":"local_only"}"#)
            .unwrap();
        assert_eq!(policy_allows(&db, &claude(&db)), Err(PolicyError::Refused));
        db.put_setting(SETTINGS_KEY, CLAUDE_OK).unwrap();
        assert_eq!(policy_allows(&db, &claude(&db)), Ok(()));
        // The right policy for another engine is still a refusal.
        db.put_setting(
            SETTINGS_KEY,
            r#"{"policy":"codex_cli","acknowledged_egress":true}"#,
        )
        .unwrap();
        assert_eq!(policy_allows(&db, &claude(&db)), Err(PolicyError::Refused));
    }

    #[test]
    fn policy_allows_reports_a_settings_change_since_resolution() {
        let mut db = db();
        db.put_setting(SETTINGS_KEY, CLAUDE_OK).unwrap();
        let engine = claude(&db);
        db.put_setting(SETTINGS_KEY, CLAUDE_OK).unwrap();
        assert_eq!(
            policy_allows(&db, &engine),
            Err(PolicyError::SettingsChanged),
            "a change away and back is still a change"
        );
    }

    #[test]
    fn a_stamp_tells_a_settings_change_from_an_inputs_change() {
        let mut db = db();
        db.put_setting(SETTINGS_KEY, CLAUDE_OK).unwrap();
        let id = db
            .meetings()
            .create(fotw_store::NewMeeting::new("dev-1", "UTC"))
            .unwrap();
        let engine = Engine::ClaudeCli {
            binary: "/does-not-exist".into(),
            settings_version: None,
        };
        let stamp = PolicyStamp::capture_typed(&db, &engine, &id).unwrap();
        assert_eq!(stamp.check_typed(&db), Ok(()));
        db.meetings()
            .create_transcript(&id, "deepgram", "nova-3", true)
            .unwrap();
        assert_eq!(stamp.check_typed(&db), Err(PolicyError::InputsChanged));
        assert!(stamp.check(&db).is_err(), "existing callers still fail");

        let stamp = PolicyStamp::capture_typed(&db, &engine, &id).unwrap();
        db.put_setting(SETTINGS_KEY, CLAUDE_OK).unwrap();
        assert_eq!(stamp.check_typed(&db), Err(PolicyError::SettingsChanged));

        assert_eq!(
            PolicyStamp::capture_typed(&db, &engine, "no-such-meeting").err(),
            Some(PolicyError::InputsChanged)
        );
        db.put_setting(SETTINGS_KEY, r#"{"policy":"off"}"#).unwrap();
        assert_eq!(
            PolicyStamp::capture_typed(&db, &engine, &id).err(),
            Some(PolicyError::Refused)
        );
        assert!(PolicyStamp::capture(&db, &engine, &id).is_err());
    }

    #[test]
    fn each_policy_error_maps_to_its_release_handling() {
        assert_eq!(
            PrepareError::from(PolicyError::Refused),
            PrepareError::PolicyRefused
        );
        assert_eq!(PrepareError::PolicyRefused.code(), "engine_unavailable");
        assert_eq!(
            PrepareError::from(PolicyError::SettingsChanged),
            PrepareError::SettingsChanged
        );
        assert_eq!(
            PrepareError::from(PolicyError::InputsChanged),
            PrepareError::InputsChanged
        );
    }
}
