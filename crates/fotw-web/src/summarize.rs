//! The seam between the UI and whatever summarises a meeting (#74).
//!
//! `fotw-web` cannot resolve a binary, read a keychain or run a subprocess and
//! must not learn how — all three live in `fotwd`. So the web layer takes a
//! trait, exactly as it takes [`GithubExport`](crate::github::GithubExport)
//! for pushes and [`RecorderControl`](crate::recorder::RecorderControl) for
//! the microphone, and the daemon supplies the implementation.
//!
//! # Why this route exists at all
//!
//! Every one of the 33 meetings in the first real library had no summary, and
//! the product offered no way to notice or to fix it: the engine could only be
//! configured from a terminal, and the dashboard rendered "engine off",
//! "engine broken" and "engine fine" as the same blank space. A settings pane
//! that cannot turn the feature on is a feature most users will never have.
//!
//! # Why the disclosure is enforced here
//!
//! [`SummarizeSettingsDoc::normalized`] refuses `cli_enabled` without
//! `acknowledged_egress`, with the stable code `disclosure_required`. That is
//! KEY-04's duty in its API shape — the exact refusal `fotwd engine
//! claude-cli` makes without `--i-acknowledge-egress`. Enforcing it in the
//! handler rather than in the form is the point: a checkbox the client can
//! decline to render is not a control, and a rule enforced in one
//! implementation is a rule the next implementation forgets.
//!
//! # KEY-01, and what is deliberately not here
//!
//! No field on this API is, contains, or could carry a secret. Keys live in
//! the OS keychain; [`SummarizeStatus::api_key_present`] says only whether one
//! is there, which is what a settings form needs to render and nothing more.

use serde::{Deserialize, Serialize};

/// Which local CLI serves as the engine.
///
/// The wire spelling matches the daemon's own persisted `cli_kind`, so the
/// settings document the UI posts and the row `fotwd` stores are the same
/// shape. Defaults to claude for rows written before codex existed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliEngine {
    /// The `claude` CLI, backed by a Claude subscription.
    #[default]
    Claude,
    /// The `codex` CLI, backed by a ChatGPT/Codex subscription.
    Codex,
}

impl CliEngine {
    /// KEY-04's disclosure for this engine, one line at a time.
    ///
    /// Served with the settings so the acknowledge checkbox sits beside the
    /// words it acknowledges rather than beside a link to them. The daemon
    /// prints the same lines from `fotwd engine`; keeping one wording is the
    /// whole reason they are a constant and not a paragraph in a template.
    #[must_use]
    pub fn disclosure(self) -> &'static [&'static str] {
        match self {
            Self::Claude => CLAUDE_DISCLOSURE,
            Self::Codex => CODEX_DISCLOSURE,
        }
    }
}

/// KEY-04 for the claude CLI: the host, what leaves, and the training default.
const CLAUDE_DISCLOSURE: &[&str] = &[
    "Enabling this sends each finished meeting's transcript to Anthropic",
    "(api.anthropic.com) through your local `claude` login. On a Claude",
    "subscription, conversations are not used for training by default;",
    "confirm current terms: https://www.anthropic.com/legal/privacy",
];

/// KEY-04 for the codex CLI.
///
/// It differs from the claude text on the point that matters most, rather than
/// implying "same as an API key": a consumer ChatGPT subscription trains on
/// conversations by default, where the Anthropic API does not.
const CODEX_DISCLOSURE: &[&str] = &[
    "Enabling this sends each finished meeting's transcript to OpenAI",
    "(chatgpt.com / api.openai.com) through your local `codex` login.",
    "NOTE: a consumer ChatGPT subscription MAY be used to train OpenAI's",
    "models by default — unlike a no-training API key. Review and opt out:",
    "https://help.openai.com/en/articles/7730893-data-controls-faq",
    "codex is also an agentic CLI: it can run shell over the transcript,",
    "so FlyOnTheWall runs it read-only with an empty HOME to shield your",
    "files. Prefer claude-cli if you want a non-agentic engine.",
];

/// The engine choice, as the UI reads and writes it.
///
/// Persisted by the daemon as JSON in the library's `settings` table, so
/// unknown fields are ignored and every field has a default — the same
/// additive-evolution rule the GitHub target and the meeting export follow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizeSettingsDoc {
    /// Whether the local CLI engine is on. Off by default: nothing leaves the
    /// machine until a person turns this on.
    pub cli_enabled: bool,
    /// Whether the user was shown, and accepted, that transcripts leave the
    /// machine through the CLI. Enablement without this is not enablement.
    pub acknowledged_egress: bool,
    /// Which CLI.
    pub cli_kind: CliEngine,
    /// The binary to run. A bare name is probed; a path is used as-is when it
    /// resolves. Empty means "the default for this engine", which the daemon
    /// stores as that engine's **bare name** rather than as today's path, so
    /// the row survives the CLI being reinstalled somewhere else (#87).
    pub binary: String,
}

impl SummarizeSettingsDoc {
    /// Validate and canonicalize what a client sent.
    ///
    /// # Errors
    ///
    /// [`SummarizeError::DisclosureRequired`] when the CLI is being turned on
    /// without the egress acknowledgement (KEY-04), or
    /// [`SummarizeError::Invalid`] with a human-readable reason.
    pub fn normalized(mut self) -> Result<Self, SummarizeError> {
        self.binary = self.binary.trim().to_owned();

        if self.cli_enabled && !self.acknowledged_egress {
            // The API shape of `--i-acknowledge-egress`. A `claude` binary on
            // the machine is not consent: half the laptops that would run this
            // tool have one for unrelated reasons, and a meeting transcript
            // silently flowing into it is the surprise this project exists to
            // not produce.
            return Err(SummarizeError::DisclosureRequired);
        }

        // A newline or a NUL in the binary would be a spawn argument nobody
        // typed. Everything else is left alone: the daemon decides whether a
        // path resolves, and it is the only thing that can.
        if self
            .binary
            .chars()
            .any(|c| c.is_control() || c == '\n' || c == '\0')
        {
            return Err(SummarizeError::Invalid(
                "the binary path has a control character in it".to_owned(),
            ));
        }
        Ok(self)
    }
}

/// What the daemon would actually do, right now.
///
/// The diagnostic that cannot lie. `fotwd engine`'s status arm used to re-run
/// the resolver *in the user's shell*, where `~/.local/bin` is on `$PATH` and a
/// bare name resolves fine — so it reported an engine the daemon could not
/// see, in exactly the case it existed to catch. This is filled in by the
/// daemon's own resolution instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizeStatus {
    /// `anthropic`, `claude-cli`, `codex-cli` or `none` — which engine a
    /// meeting finishing now would get.
    pub engine: String,
    /// Whether the configured binary resolves for the daemon.
    pub binary_resolves: bool,
    /// What the settings row holds, verbatim.
    pub configured_binary: String,
    /// What the daemon resolved it to, when it resolved to anything.
    ///
    /// Reported beside `configured_binary` rather than instead of it: they
    /// differ whenever a bare name was probed or a stale absolute path was
    /// rescued by its basename, and a status that showed only one of them
    /// would name a binary the daemon is not running.
    pub resolved_binary: Option<String>,
    /// Whether an Anthropic API key is in the keychain. **Presence only** —
    /// KEY-01 keeps the key itself off every surface, including this one.
    pub api_key_present: bool,
    /// KEY-04's disclosure for **every** engine, keyed by its wire spelling
    /// (`claude`, `codex`), one line at a time.
    ///
    /// Every engine rather than the stored one, because the words have to
    /// match what the user is about to save, not what is saved now. A picker
    /// that switched to codex while still showing Anthropic's "not used for
    /// training by default" would collect an acknowledgement of the wrong
    /// facts — which is the one failure mode a disclosure has.
    pub disclosures: std::collections::BTreeMap<String, Vec<String>>,
}

impl SummarizeStatus {
    /// Every engine's disclosure, keyed by wire spelling.
    ///
    /// The shape [`SummarizeStatus::disclosures`] wants, built once here so no
    /// implementation has to remember which engines exist.
    #[must_use]
    pub fn all_disclosures() -> std::collections::BTreeMap<String, Vec<String>> {
        [CliEngine::Claude, CliEngine::Codex]
            .into_iter()
            .map(|kind| {
                let key = serde_json::to_value(kind)
                    .ok()
                    .and_then(|v| v.as_str().map(ToOwned::to_owned))
                    .unwrap_or_default();
                let lines = kind.disclosure().iter().map(|l| (*l).to_owned()).collect();
                (key, lines)
            })
            .collect()
    }
}

/// Why a settings save did not do what was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SummarizeError {
    /// The CLI was enabled without the egress acknowledgement (KEY-04). The
    /// UI's cue to show the disclosure and the checkbox.
    #[error("disclosure_required")]
    DisclosureRequired,
    /// The settings were refused; the string says why.
    #[error("invalid_settings: {0}")]
    Invalid(String),
    /// Everything else, in words safe to show. Never transcript text.
    #[error("{0}")]
    Failed(String),
}

/// Anything that can store the engine choice and report what it resolves to.
///
/// `Send + Sync + 'static` because the handlers hand it to
/// [`tokio::task::spawn_blocking`]: reading the library and the keychain both
/// block.
pub trait SummarizeControl: Send + Sync + 'static {
    /// The engine choice in force. Falls back to the default rather than
    /// failing: a missing row is a fresh library, not an error.
    fn settings(&self) -> SummarizeSettingsDoc;

    /// Persist new settings, already validated by
    /// [`SummarizeSettingsDoc::normalized`], and return what was stored.
    ///
    /// # Errors
    ///
    /// [`SummarizeError::Failed`] if the store refused the write.
    fn set_settings(
        &self,
        settings: SummarizeSettingsDoc,
    ) -> Result<SummarizeSettingsDoc, SummarizeError>;

    /// What this daemon would resolve, right now.
    fn status(&self) -> SummarizeStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(acknowledged: bool) -> SummarizeSettingsDoc {
        SummarizeSettingsDoc {
            cli_enabled: true,
            acknowledged_egress: acknowledged,
            cli_kind: CliEngine::Claude,
            binary: "claude".to_owned(),
        }
    }

    #[test]
    fn enabling_without_the_acknowledgement_is_refused_by_its_own_code() {
        assert_eq!(
            enabled(false).normalized().unwrap_err(),
            SummarizeError::DisclosureRequired
        );
        assert!(enabled(true).normalized().is_ok());
    }

    /// Off is always allowed: nothing is leaving, so there is nothing to
    /// acknowledge, and refusing here would make the engine impossible to
    /// switch back off from the UI.
    #[test]
    fn switching_off_never_needs_an_acknowledgement() {
        let off = SummarizeSettingsDoc::default();
        assert!(off.normalized().is_ok());
    }

    #[test]
    fn the_binary_is_trimmed_and_control_characters_are_refused() {
        let padded = SummarizeSettingsDoc {
            binary: "  /opt/homebrew/bin/claude  ".to_owned(),
            ..enabled(true)
        };
        assert_eq!(
            padded.normalized().unwrap().binary,
            "/opt/homebrew/bin/claude"
        );

        let sneaky = SummarizeSettingsDoc {
            binary: "claude\nrm -rf".to_owned(),
            ..enabled(true)
        };
        assert!(matches!(
            sneaky.normalized(),
            Err(SummarizeError::Invalid(_))
        ));
    }

    #[test]
    fn error_codes_are_stable_wire_strings() {
        assert_eq!(
            SummarizeError::DisclosureRequired.to_string(),
            "disclosure_required"
        );
        assert_eq!(
            SummarizeError::Invalid("why".to_owned()).to_string(),
            "invalid_settings: why"
        );
    }

    /// A settings row written before `cli_kind` existed reads back as claude,
    /// never as an error — the same additive rule the GitHub document follows.
    #[test]
    fn an_older_settings_document_still_parses() {
        let doc: SummarizeSettingsDoc =
            serde_json::from_str(r#"{"cli_enabled":true,"acknowledged_egress":true}"#).unwrap();
        assert_eq!(doc.cli_kind, CliEngine::Claude);
        assert!(doc.binary.is_empty());
    }

    #[test]
    fn each_disclosure_names_its_host_and_its_training_default() {
        let claude = CliEngine::Claude.disclosure().join(" ");
        assert!(claude.contains("api.anthropic.com"));
        assert!(claude.contains("not used for training by default"));

        let codex = CliEngine::Codex.disclosure().join(" ");
        assert!(codex.contains("api.openai.com"));
        assert!(codex.contains("train OpenAI"));
    }

    /// Keyed by the same strings the settings document uses for `cli_kind`,
    /// so the client can index one by the other without a lookup table.
    #[test]
    fn every_engine_has_a_disclosure_under_its_own_wire_spelling() {
        let all = SummarizeStatus::all_disclosures();
        assert_eq!(all.len(), 2);
        assert!(all["claude"].join(" ").contains("anthropic.com"));
        assert!(all["codex"].join(" ").contains("openai.com"));
    }
}
