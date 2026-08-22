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
            auto_since_ms: None,
        }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubReceipt {
    /// `owner/name` it went to.
    pub repo: String,
    /// The file inside the repository.
    pub path: String,
    /// The commit the Contents API answered with.
    pub commit: String,
    /// When, epoch milliseconds.
    pub pushed_at_ms: u64,
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

    /// The repositories this login may push to, `owner/name`, most recently
    /// active first — what the settings form offers instead of a blank field.
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
    fn error_codes_are_stable_wire_strings() {
        assert_eq!(GithubError::GhMissing.to_string(), "gh_missing");
        assert_eq!(
            GithubError::NotAuthenticated.to_string(),
            "gh_not_authenticated"
        );
        assert_eq!(GithubError::RepoNotFound.to_string(), "repo_not_found");
        assert_eq!(GithubError::Disabled.to_string(), "github_export_disabled");
        assert_eq!(
            GithubError::Invalid("why".to_owned()).to_string(),
            "invalid_settings: why"
        );
    }
}
