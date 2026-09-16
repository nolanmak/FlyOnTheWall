//! The seam between the UI and whatever pushes a transcript to GitHub.
//!
//! `fotw-web` cannot run `gh` and must not learn how — subprocesses, the
//! library and the audit log all live in `fotwd`. So the web layer takes a
//! trait, exactly as it takes [`RecorderControl`](crate::recorder::RecorderControl)
//! for the microphone, and `fotwd` supplies the implementation (issue #63).
//!
//! # Why the errors are strings in a 200 body
//!
//! "gh is not installed", "gh is not logged in" and "that repo does not
//! exist" are facts about this machine and this user's accounts. ING-09
//! withholds facts from callers without the bearer, and a status code that
//! varied with them would hand a scanning page a bit of the answer. The HTTP
//! layer says only whether the request was well-formed and authorised; what
//! the pusher found is in the body.
//!
//! # Why validation lives here
//!
//! [`GithubSettings::normalized`] runs in the handler, before the trait is
//! called, so every implementation — the daemon's real one and every test
//! fake — receives only settings that already passed. A rule enforced in an
//! implementation is a rule the next implementation forgets.

use serde::{Deserialize, Serialize};

/// When a transcript is pushed.
///
/// The wire spelling is part of the UI contract, so it is pinned by a test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubMode {
    /// Only when the user presses the per-meeting button. The default:
    /// automatic egress of meeting content should be an opt-in inside an
    /// opt-in.
    #[default]
    Manual,
    /// Also when a meeting finishes.
    Auto,
}

/// The GitHub export target, as the UI reads and writes it.
///
/// Persisted by the daemon as JSON in the library's `settings` table, so
/// unknown fields are ignored and every field has a default — the same
/// additive-evolution rule the meeting export document follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubSettings {
    /// Whether pushing is allowed at all. Off by default: nothing leaves the
    /// machine until a person turns this on.
    pub enabled: bool,
    /// `owner/name` on github.com.
    pub repo: String,
    /// Branch to commit to; empty means the repository's default branch.
    pub branch: String,
    /// Directory the transcripts land in, `""` for the repository root.
    /// Stored without a leading slash and with a trailing one.
    pub path_prefix: String,
    /// When a push happens.
    pub mode: GithubMode,
    /// The user's acknowledgement that `repo` may be public. Off by default:
    /// without it, the daemon refuses to push to a repository GitHub does not
    /// confirm is private ([`GithubError::RepoIsPublic`]). Anything pushed to
    /// a public repository is readable by anyone, and its history keeps it
    /// after the file is deleted.
    ///
    /// A row stored before this field existed has no such key and reads as
    /// `false`, through the struct's `#[serde(default)]`.
    pub allow_public_repo: bool,
    /// Whether auto mode owes **every** ready meeting, rather than only those
    /// that started after [`GithubSettings::auto_since_ms`].
    ///
    /// Off by default, and a row stored before this field existed reads as
    /// `false` through the struct's `#[serde(default)]` — so the stamp keeps
    /// bounding auto for every library that has one, and nobody publishes an
    /// archive by upgrading.
    ///
    /// On, it publishes the whole library: every meeting already recorded, not
    /// just the ones recorded from now on. That is why it is a second switch
    /// beside `mode` rather than a widening of it — "push new meetings" and
    /// "push everything I have ever recorded" are different decisions, and the
    /// second one cannot be taken back, because a commit stays in the
    /// repository's history.
    ///
    /// The stamp is still stored and still honoured the moment this goes back
    /// off, so turning it off returns auto to "meetings from now on" rather
    /// than to "nothing".
    pub sync_whole_library: bool,
    /// When auto mode was switched on, epoch milliseconds.
    ///
    /// Server-owned: the daemon stamps it so that enabling auto on an old
    /// library pushes future meetings, not the whole archive. Whatever a
    /// client sends here is discarded by [`GithubSettings::normalized`].
    pub auto_since_ms: Option<u64>,
}

impl Default for GithubSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            repo: String::new(),
            branch: String::new(),
            path_prefix: "meetings/".to_owned(),
            mode: GithubMode::Manual,
            allow_public_repo: false,
            sync_whole_library: false,
            auto_since_ms: None,
        }
    }
}

/// Where one meeting stands with the GitHub target (issue #112).
///
/// Exactly one of these is true of a meeting at any moment, and that is the
/// point. The dashboard used to draw a push button from the settings alone, so
/// "already in the repository" and "never pushed" looked identical on screen
/// and the button invited a second commit of a meeting that had already landed.
///
/// The wire spelling is part of the UI contract, as [`GithubMode`]'s is, and a
/// test pins it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubSyncState {
    /// Export is switched off, or this build has no GitHub control at all.
    /// Nothing is said about the meeting, because nothing is true of it.
    #[default]
    Off,
    /// Enabled, and this meeting has never been pushed.
    Never,
    /// Pushed, and nothing has changed since.
    Synced,
    /// Pushed, and then the summary or the saved document brief changed, so the
    /// copy in the repository is older than the library's. The worker will send
    /// it again on a later pass.
    Changed,
    /// The last attempt failed. The only state that offers a control.
    Failed,
}

/// A [`GithubSyncState`] with the facts that make the line worth reading.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubSyncStatus {
    /// Which state this meeting is in.
    pub state: GithubSyncState,
    /// `owner/name` it was last pushed to, when it has been.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// When that push landed, epoch milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pushed_at_ms: Option<u64>,
    /// Why the last attempt failed, in words safe to show — `gh`'s own first
    /// line about a repository, never transcript text. Present only for
    /// [`GithubSyncState::Failed`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the worker will try this meeting again on its own, epoch
    /// milliseconds. Present only for [`GithubSyncState::Failed`] **and only
    /// when the worker will in fact act**: an automatic retry belongs to auto
    /// mode, and a meeting outside what auto mode owes is retried by nobody.
    /// Promising otherwise is the same lie as the button that had nothing to do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at_ms: Option<u64>,
    /// Whether the auto worker will push this meeting on its own (#112).
    ///
    /// False for a meeting no automatic pass will ever reach: manual mode, or a
    /// meeting that started before [`GithubSettings::auto_since_ms`] while
    /// [`GithubSettings::sync_whole_library`] is off. The worker bounds what it
    /// owes by all three of `mode`, the stamp and the switch, so a state that
    /// consulted none of them reported the same thing for a meeting about to be
    /// pushed and one that will never be touched — and the dashboard turned that
    /// into a promise the daemon does not keep. The one control the pane offers
    /// is drawn for exactly the meetings this is false for.
    pub scheduled: bool,
    /// The last refusal that answers for *every* meeting, as the stable code
    /// from [`GithubError`] — `gh_missing`, `gh_not_authenticated`,
    /// `repo_not_found`, `github_export_disabled` or `repo_is_public` (#112).
    ///
    /// Present only for a meeting the worker owes and cannot push. None of these
    /// is one meeting's fault, so no meeting enters a retry window for them;
    /// before this the round logged the reason and every meeting went on saying
    /// "not synced yet" forever, with the log the only place that knew why.
    /// Nothing here promises an automatic retry, and the first push that lands
    /// clears it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    /// When that refusal was last seen, epoch milliseconds — how fresh the
    /// reason beside it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_at_ms: Option<u64>,
}

impl GithubSyncStatus {
    /// The answer for a target that is switched off — and, being the default,
    /// the base every other state is built from.
    #[must_use]
    pub fn off() -> Self {
        Self::default()
    }
}

impl GithubSettings {
    /// Validate and canonicalize what a client sent.
    ///
    /// # Errors
    ///
    /// A human-readable reason, carried to the UI as
    /// `invalid_settings: <reason>` beside a 200.
    pub fn normalized(mut self) -> Result<Self, String> {
        // The stamp is the daemon's to manage, never the client's.
        self.auto_since_ms = None;

        self.repo = self.repo.trim().to_owned();
        if self.enabled || !self.repo.is_empty() {
            validate_repo(&self.repo)?;
        }

        self.branch = self.branch.trim().to_owned();
        if self
            .branch
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("the branch name has whitespace in it".to_owned());
        }
        // The branch rides in a URL the gh CLI hands to Go's URL parser,
        // which treats '#' as a fragment, '?' as a query, '%' as an escape
        // and '&' as a separator — all legal in a git refname, all silently
        // changing which file gets written. Verified against the real
        // parser: `repos/o/r/contents/x#y` fetches `x`.
        if self.branch.chars().any(url_metacharacter) {
            return Err(
                "the branch name has a character (#, ?, %, &) that a URL would misread".to_owned(),
            );
        }

        self.path_prefix = normalize_prefix(self.path_prefix.trim())?;
        Ok(self)
    }
}

/// `owner/name`, in the character set GitHub itself accepts.
fn validate_repo(repo: &str) -> Result<(), String> {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!("`{repo}` is not owner/name"));
    };
    if owner.is_empty() || name.is_empty() {
        return Err(format!("`{repo}` is not owner/name"));
    }
    // Owners (users and orgs) are alphanumeric plus '-' and '_'; only repo
    // names may carry dots. "." and ".." are how `repos/../gists` walks out
    // of the /repos/ namespace entirely — GitHub's server normalizes dot
    // segments — so a name of only dots is refused outright.
    let owner_ok = owner
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    let name_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.chars().all(|c| c == '.');
    if !owner_ok || !name_ok {
        return Err(format!("`{repo}` has characters GitHub would refuse"));
    }
    Ok(())
}

/// A character Go's URL parser would reinterpret before GitHub ever sees it.
fn url_metacharacter(c: char) -> bool {
    matches!(c, '#' | '?' | '%' | '&')
}

/// No leading slash, a trailing one unless empty, and no way out of the repo.
fn normalize_prefix(prefix: &str) -> Result<String, String> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.contains('\\') {
        return Err("the path prefix has a backslash in it".to_owned());
    }
    for segment in trimmed.split('/') {
        if segment.is_empty() {
            return Err("the path prefix has an empty segment".to_owned());
        }
        if segment == "." || segment == ".." {
            return Err("the path prefix must stay inside the repository".to_owned());
        }
        // Same reason as the branch: '#' truncates the URL at the parser,
        // so a prefix of "q#a/" would commit every transcript to a root
        // file named "q", each push overwriting the last.
        if segment.chars().any(url_metacharacter) {
            return Err(
                "the path prefix has a character (#, ?, %, &) that a URL would misread".to_owned(),
            );
        }
    }
    Ok(format!("{trimmed}/"))
}

/// Proof one transcript landed: enough to find the commit again, and the
/// stable path a re-push updates rather than duplicating.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubReceipt {
    /// `owner/name` it went to.
    pub repo: String,
    /// The file inside the repository.
    pub path: String,
    /// The commit the Contents API answered with.
    pub commit: String,
    /// When, epoch milliseconds.
    pub pushed_at_ms: u64,
    /// The meeting's title at push time, so the bundle's `index.md`/`log.md`
    /// can label the link without re-reading the library. Defaults empty for
    /// receipts written before the OKF bundle existed.
    pub title: String,
    /// The meeting's start, epoch milliseconds, so the listing can date and
    /// order entries. Defaults 0 for pre-bundle receipts.
    pub started_at_ms: u64,
}

/// Why a push or a save did not do what was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GithubError {
    /// The meeting id names nothing. The handler turns this into the same
    /// bare 404 `GET /api/meetings/{id}` answers, for the same reason.
    #[error("no_such_meeting")]
    NoSuchMeeting,
    /// The target is switched off; nothing was contacted.
    #[error("github_export_disabled")]
    Disabled,
    /// No `gh` binary anywhere we looked.
    #[error("gh_missing")]
    GhMissing,
    /// `gh auth status` failed: there is a binary but no usable login.
    #[error("gh_not_authenticated")]
    NotAuthenticated,
    /// The configured repository is not reachable with this login.
    #[error("repo_not_found")]
    RepoNotFound,
    /// The repository is public, or GitHub did not confirm it is private, and
    /// [`GithubSettings::allow_public_repo`] is off. Refused before anything
    /// was written.
    #[error("repo_is_public")]
    RepoIsPublic,
    /// The settings were refused; the string says why.
    #[error("invalid_settings: {0}")]
    Invalid(String),
    /// Everything else, in words safe to show. Never transcript text.
    #[error("{0}")]
    Failed(String),
}

/// Anything that can store the target and push a transcript on the UI's
/// behalf.
///
/// `Send + Sync + 'static` because the handlers hand it to
/// [`tokio::task::spawn_blocking`]: reading the library blocks, and a push is
/// a subprocess making network calls.
pub trait GithubExport: Send + Sync + 'static {
    /// The target in force. Falls back to [`GithubSettings::default`] rather
    /// than failing: a missing row is a fresh library, not an error.
    fn settings(&self) -> GithubSettings;

    /// Persist new settings, already validated by
    /// [`GithubSettings::normalized`], and return what was stored — the
    /// implementation may stamp [`GithubSettings::auto_since_ms`].
    ///
    /// # Errors
    ///
    /// [`GithubError::Failed`] if the store refused the write.
    fn set_settings(&self, settings: GithubSettings) -> Result<GithubSettings, GithubError>;

    /// Repositories this login may push to, `owner/name`, most recently
    /// active first — what the settings form offers instead of a blank field.
    /// Not necessarily all of them: the daemon's implementation leaves public
    /// repositories out, because this list has no field to label one with.
    ///
    /// # Errors
    ///
    /// [`GithubError::GhMissing`], [`GithubError::NotAuthenticated`], or
    /// [`GithubError::Failed`] — the same states a push would have hit, found
    /// before anything was configured.
    fn repos(&self) -> Result<Vec<String>, GithubError>;

    /// Commit one meeting's Markdown export to the configured repository.
    ///
    /// # Errors
    ///
    /// The full taxonomy in [`GithubError`]; each variant renders differently
    /// in the UI.
    fn push(&self, meeting_id: &str) -> Result<GithubReceipt, GithubError>;

    /// Where `meeting_id` stands with the target right now (issue #112).
    ///
    /// Infallible on purpose: it answers from bookkeeping the implementation
    /// already holds — the push receipts, the companion versions and the
    /// worker's retry table — and contacts nothing. A meeting id that names
    /// nothing answers [`GithubSyncState::Never`] rather than an error, because
    /// a route that distinguished the two would let a caller confirm a guessed
    /// id (ING-09).
    fn sync_status(&self, meeting_id: &str) -> GithubSyncStatus;

    /// Regenerate the OKF bundle's `index.md` and `log.md` from what has been
    /// pushed and commit them, so the repo is a navigable bundle rather than a
    /// flat pile of files. Called after a push, not inside it: a manual push
    /// syncs once, and the auto worker syncs once per batch rather than once
    /// per meeting.
    ///
    /// # Errors
    ///
    /// The same taxonomy as [`GithubExport::push`]. Callers treat it as
    /// best-effort — the meeting files already landed.
    fn sync_bundle(&self) -> Result<(), GithubError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(repo: &str, prefix: &str) -> GithubSettings {
        GithubSettings {
            enabled: true,
            repo: repo.to_owned(),
            path_prefix: prefix.to_owned(),
            ..GithubSettings::default()
        }
    }

    #[test]
    fn a_repo_must_be_owner_slash_name() {
        assert!(enabled("octocat/notes", "m/").normalized().is_ok());
        assert!(enabled("octocat", "m/").normalized().is_err());
        assert!(enabled("a/b/c", "m/").normalized().is_err());
        assert!(
            enabled("", "m/").normalized().is_err(),
            "enabled needs a repo"
        );
        assert!(enabled("owner/has space", "m/").normalized().is_err());
    }

    #[test]
    fn url_metacharacters_are_refused_everywhere_they_could_reroute_a_push() {
        // Verified against gh: '#' makes Go's URL parser drop the rest of
        // the path, so these are not pedantry — each one silently writes to
        // a file the user never named.
        for bad in ["q#a/", "q?a/", "q%2fa/", "q&a/"] {
            assert!(
                enabled("o/n", bad).normalized().is_err(),
                "prefix {bad:?} must be refused"
            );
        }
        for bad in ["feat#1", "feat?x", "feat%31", "a&b"] {
            let s = GithubSettings {
                branch: bad.to_owned(),
                ..enabled("o/n", "m/")
            };
            assert!(s.normalized().is_err(), "branch {bad:?} must be refused");
        }
        // The characters GitHub itself uses stay legal.
        assert!(enabled("o/n", "notes/meetings/").normalized().is_ok());
        let fine = GithubSettings {
            branch: "feat/x-1.2_ok".to_owned(),
            ..enabled("o/n", "m/")
        };
        assert!(fine.normalized().is_ok());
    }

    #[test]
    fn dot_segments_cannot_walk_out_of_the_repos_namespace() {
        // `repos/../gists` is a real, reachable endpoint after the server
        // normalizes the dots. An owner never contains a dot at all.
        assert!(enabled("../gists", "m/").normalized().is_err());
        assert!(enabled("./x", "m/").normalized().is_err());
        assert!(enabled("o/..", "m/").normalized().is_err());
        assert!(enabled("o/.", "m/").normalized().is_err());
        assert!(enabled("dotted.owner/x", "m/").normalized().is_err());
        assert!(enabled("o/repo.name", "m/").normalized().is_ok());
    }

    #[test]
    fn a_disabled_target_may_be_empty_but_not_malformed() {
        assert!(GithubSettings::default().normalized().is_ok());
        let half_typed = GithubSettings {
            repo: "octocat".to_owned(),
            ..GithubSettings::default()
        };
        assert!(
            half_typed.normalized().is_err(),
            "a wrong repo is wrong even while disabled — saving it silently \
             is how it gets enabled later without another look"
        );
    }

    #[test]
    fn the_prefix_is_canonicalized() {
        let n = |p: &str| enabled("o/n", p).normalized().map(|s| s.path_prefix);
        assert_eq!(n("meetings").unwrap(), "meetings/");
        assert_eq!(n("/notes/meetings/").unwrap(), "notes/meetings/");
        assert_eq!(n("").unwrap(), "");
        assert_eq!(n("/").unwrap(), "");
        assert!(n("../up").is_err());
        assert!(n("a/../b").is_err());
        assert!(n("a//b").is_err());
        assert!(n("a\\b").is_err());
    }

    #[test]
    fn the_stamp_a_client_sends_is_discarded() {
        let s = GithubSettings {
            auto_since_ms: Some(12345),
            ..GithubSettings::default()
        };
        assert_eq!(s.normalized().unwrap().auto_since_ms, None);
    }

    #[test]
    fn a_row_stored_before_the_acknowledgement_existed_reads_as_not_given() {
        // The shape every library stored before `allow_public_repo` was added.
        let old: GithubSettings = serde_json::from_str(
            r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"auto","auto_since_ms":1}"#,
        )
        .unwrap();
        assert!(!old.allow_public_repo);
        assert_eq!(
            old.repo, "octocat/notes",
            "the rest of the row still parses"
        );
        assert!(!GithubSettings::default().allow_public_repo);
    }

    #[test]
    fn the_acknowledgement_survives_validation_and_serialization() {
        let s = GithubSettings {
            allow_public_repo: true,
            ..enabled("octocat/notes", "m/")
        };
        let normalized = s.normalized().unwrap();
        assert!(normalized.allow_public_repo, "normalized() must keep it");
        let json = serde_json::to_value(&normalized).unwrap();
        assert_eq!(
            json["allow_public_repo"], true,
            "the wire name the settings form reads and writes"
        );
        let back: GithubSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back, normalized);
    }

    /// A4, at the type level: nothing about upgrading turns the whole-library
    /// switch on, and the stamp that bounds auto is still there beside it.
    #[test]
    fn the_whole_library_switch_is_off_by_default_and_in_every_older_row() {
        assert!(!GithubSettings::default().sync_whole_library);
        // The shape every library stored before this field was added.
        let old: GithubSettings = serde_json::from_str(
            r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"auto","auto_since_ms":1787372196265}"#,
        )
        .unwrap();
        assert!(
            !old.sync_whole_library,
            "an upgrade must never publish an existing archive"
        );
        assert_eq!(
            old.auto_since_ms,
            Some(1_787_372_196_265),
            "and the stamp that bounds auto is untouched"
        );
    }

    #[test]
    fn the_whole_library_switch_survives_validation_and_serialization() {
        let s = GithubSettings {
            sync_whole_library: true,
            ..enabled("octocat/notes", "m/")
        };
        let normalized = s.clone().normalized().unwrap();
        assert!(normalized.sync_whole_library, "normalized() must keep it");
        let json = serde_json::to_value(&normalized).unwrap();
        assert_eq!(
            json["sync_whole_library"], true,
            "the wire name the settings form reads and writes"
        );
        let back: GithubSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back, normalized);
    }

    #[test]
    fn the_sync_state_spellings_are_pinned() {
        for (state, wire) in [
            (GithubSyncState::Off, r#""off""#),
            (GithubSyncState::Never, r#""never""#),
            (GithubSyncState::Synced, r#""synced""#),
            (GithubSyncState::Changed, r#""changed""#),
            (GithubSyncState::Failed, r#""failed""#),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
        }
        assert_eq!(
            GithubSyncState::default(),
            GithubSyncState::Off,
            "a control that says nothing yet must not claim a meeting is synced"
        );
    }

    /// A field that is absent means "this state has no such fact", and the UI
    /// branches on presence. A serialized `null` would read as present.
    #[test]
    fn a_sync_status_omits_the_fields_its_state_does_not_have() {
        let off = serde_json::to_value(GithubSyncStatus::off()).unwrap();
        assert_eq!(off["state"], "off");
        for absent in ["repo", "pushed_at_ms", "error", "retry_at_ms"] {
            assert!(
                off.get(absent).is_none(),
                "{absent} must be omitted rather than null"
            );
        }

        let failed = GithubSyncStatus {
            state: GithubSyncState::Failed,
            repo: Some("octocat/notes".to_owned()),
            pushed_at_ms: None,
            error: Some("gh: Validation Failed (HTTP 422)".to_owned()),
            retry_at_ms: Some(1_787_372_496_265),
            ..GithubSyncStatus::off()
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["state"], "failed");
        assert_eq!(json["error"], "gh: Validation Failed (HTTP 422)");
        assert_eq!(json["retry_at_ms"], 1_787_372_496_265_u64);
        assert!(json.get("pushed_at_ms").is_none());
    }

    /// F1, at the wire level. A status written by a build that had no such key
    /// described a worker that owed every meeting, so the absent key has to read
    /// as `true`: the false answer is the one that draws a control, and a
    /// control drawn beside a worker that is already going to push is the
    /// duplicate commit this issue exists to stop.
    #[test]
    fn a_status_with_no_scheduled_key_reads_as_scheduled() {
        let old: GithubSyncStatus = serde_json::from_str(r#"{"state":"never"}"#).unwrap();
        assert!(
            old.scheduled,
            "a status from before the field existed must not sprout a control"
        );
        assert_eq!(old.state, GithubSyncState::Never);
        assert!(
            !GithubSyncStatus::off().scheduled,
            "the deliberate exception: a switched-off target pushes nothing at all"
        );

        // And the key is always written, so a `false` cannot read back as true.
        let by_hand = GithubSyncStatus {
            state: GithubSyncState::Never,
            scheduled: false,
            ..GithubSyncStatus::off()
        };
        let json = serde_json::to_value(&by_hand).unwrap();
        assert_eq!(json["scheduled"], false);
        assert_eq!(
            serde_json::from_value::<GithubSyncStatus>(json).unwrap(),
            by_hand
        );
    }

    /// F4, at the wire level. The refusal that answers for every meeting travels
    /// as the same stable code the dashboard's error table is already keyed by,
    /// and carries no retry time: nothing is scheduled to retry it.
    #[test]
    fn a_blocked_status_carries_an_error_code_and_no_retry_time() {
        let blocked = GithubSyncStatus {
            state: GithubSyncState::Never,
            blocked: Some(GithubError::NotAuthenticated.to_string()),
            blocked_at_ms: Some(1_787_372_196_265),
            ..GithubSyncStatus::off()
        };
        let json = serde_json::to_value(&blocked).unwrap();
        assert_eq!(json["blocked"], "gh_not_authenticated");
        assert_eq!(json["blocked_at_ms"], 1_787_372_196_265_u64);
        assert!(
            json.get("retry_at_ms").is_none(),
            "an environment failure parks no meeting, so nothing will retry it"
        );

        let off = serde_json::to_value(GithubSyncStatus::off()).unwrap();
        for absent in ["blocked", "blocked_at_ms"] {
            assert!(
                off.get(absent).is_none(),
                "{absent} must be omitted rather than null"
            );
        }
    }

    #[test]
    fn error_codes_are_stable_wire_strings() {
        assert_eq!(GithubError::GhMissing.to_string(), "gh_missing");
        assert_eq!(
            GithubError::NotAuthenticated.to_string(),
            "gh_not_authenticated"
        );
        assert_eq!(GithubError::RepoNotFound.to_string(), "repo_not_found");
        // The UI's GH_ERRORS table is keyed by this exact string.
        assert_eq!(GithubError::RepoIsPublic.to_string(), "repo_is_public");
        assert_eq!(GithubError::Disabled.to_string(), "github_export_disabled");
        assert_eq!(
            GithubError::Invalid("why".to_owned()).to_string(),
            "invalid_settings: why"
        );
    }
}
