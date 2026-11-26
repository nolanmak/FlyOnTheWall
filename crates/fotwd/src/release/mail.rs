//! The mail transport for attendee release (#326 D12, 5.5): Google Workspace
//! CLI (`gws`), invoked the way [`crate::github`] invokes `gh`.
//!
//! # Why `gws`, and why a trait
//!
//! The message goes from this machine straight to the mail provider, through a
//! CLI the user already authenticated, with no relay in between (N5). The
//! [`MailRunner`] trait exists for the same reason [`crate::github::GhRunner`]
//! does: the real thing spawns a process that talks to the network, and every
//! ordinary test would rather script it.
//!
//! # How the process is run
//!
//! An argument vector, never a shell string. Every free-text value uses the
//! `--flag=value` form, so a subject or body that starts with `-` is a value
//! and never a flag. The child gets `GOOGLE_WORKSPACE_CLI_CONFIG_DIR` set to
//! the configured profile and every other inherited `GOOGLE_WORKSPACE_*`
//! variable removed except the keyring backend choice (a profile made with
//! the file backend needs it to read its own credentials), so a token or log
//! directory in the daemon's environment can neither change the sending
//! account nor copy the request into a log file. It runs in its own process group with a 120 s limit; on timeout the
//! whole group is killed, so an npm install's `node` grandchild dies with it.
//!
//! # What comes back
//!
//! Codes, never text (D15). `gws` stderr is read only far enough to pick a
//! code and then dropped; nothing here formats an address, a subject, a body
//! or stderr into an error or a log line.
//!
//! # OT1: what `gws` 0.22.5 prints
//!
//! Recorded with `--dry-run`, and with a send attempt from a fresh, empty
//! profile directory that Google refused (no message was sent). Field names
//! only:
//!
//! - `--dry-run` prints `{body, dry_run, is_multipart_upload, method,
//!   query_params, url}` and exits 0.
//! - Any failure prints `{"error": {code, message, reason}}` on **stdout**,
//!   a `error[<kind>]: …` line on stderr, and exits with the kind's status:
//!   1 API error, 2 auth, 3 validation, 4 discovery, 5 internal. A Google
//!   rejection of the send request (HTTP 403 `insufficientPermissions`) came
//!   back as exit 1 with `error.code` = 403.
//! - A successful send was not recorded here (that is the manual
//!   `release_live` run). The Gmail API's `messages.send` returns a Message
//!   resource whose `id` is the message id, so `id` is read when present and
//!   a success without it is still a success.
//! - The empty profile directory still reached Google with *a* credential
//!   (403 for scopes, not exit 2), so the directory is not proof of which
//!   account sends. The preflight's `getProfile` and its sender equality check
//!   are what pin the account.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{SendPermit, address_allowed};

/// How long one `gws` call may run before its process group is killed.
pub const MAIL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a passing preflight is trusted (5.3).
pub const PREFLIGHT_TTL_MS: i64 = 10 * 60 * 1_000;

/// The variable that points `gws` at one profile directory.
pub const CONFIG_DIR_ENV: &str = "GOOGLE_WORKSPACE_CLI_CONFIG_DIR";

/// The one other `GOOGLE_WORKSPACE_*` variable the child keeps.
const KEYRING_BACKEND_ENV: &str = "GOOGLE_WORKSPACE_CLI_KEYRING_BACKEND";

/// Where `gws` runs from: the profile, and the binary when PATH cannot see it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct MailConfig {
    /// `gws_profile_dir`: the `GOOGLE_WORKSPACE_CLI_CONFIG_DIR` for every call.
    pub profile_dir: PathBuf,
    /// `gws_binary`: an explicit path, for an npm install the daemon's `PATH`
    /// cannot see. `None` resolves like `resolve_gh`.
    pub binary: Option<PathBuf>,
}

/// One outgoing email.
#[derive(Debug, Clone, Copy)]
pub struct MailMessage<'a> {
    /// The verified sender.
    pub from: &'a str,
    /// Every recipient, all in `To:` (D9).
    pub to: &'a [String],
    /// The subject.
    pub subject: &'a str,
    /// The plain-text body; empty for the owner notice.
    pub body: &'a str,
    /// The attachment's temp file; `None` for the owner notice.
    pub attachment: Option<&'a Path>,
}

/// Why a `gws` call did not succeed. Codes only: no variant carries text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailError {
    /// The OS could not start the process (or there is no binary). No
    /// process ran, so nothing can have been sent.
    SpawnFailed,
    /// Google rejected the request (exit 1 with a 4xx `error.code`, per OT1).
    Refused {
        /// The HTTP status from `error.code`.
        http_status: u16,
    },
    /// The 120 s limit passed and the process group was killed.
    Timeout,
    /// Anything else: an unrecognised failure whose outcome is unknown.
    Other {
        /// A fixed code, e.g. `exit_auth` or `exit_signal`.
        code: &'static str,
    },
}

impl MailError {
    /// The code this error is logged and stored as.
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            Self::SpawnFailed => "spawn_failed".to_owned(),
            Self::Refused { http_status } => format!("refused_http_{http_status}"),
            Self::Timeout => "timeout".to_owned(),
            Self::Other { code } => (*code).to_owned(),
        }
    }
}

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.code())
    }
}

/// How `gws` gets run.
pub trait MailRunner: Send + Sync {
    /// Send `message`. Only [`super::send_one`] and [`super::send_notice`]
    /// can call this: a [`SendPermit`] is minted nowhere else, and a test
    /// scans the crate for any other caller.
    ///
    /// Returns the message id when `gws` printed one.
    ///
    /// # Errors
    ///
    /// [`MailError`], mapped by the caller as D13 says.
    fn send(
        &self,
        permit: SendPermit,
        config: &MailConfig,
        message: &MailMessage<'_>,
    ) -> Result<Option<String>, MailError>;

    /// The account's address, from `gws gmail users getProfile`.
    ///
    /// # Errors
    ///
    /// [`MailError`]; any error fails the preflight.
    fn profile_address(&self, config: &MailConfig) -> Result<String, MailError>;
}

/// The argument vector for a send (5.5), without the program.
#[must_use]
pub fn send_argv(message: &MailMessage<'_>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        "gmail".into(),
        "+send".into(),
        format!("--from={}", message.from).into(),
        format!("--to={}", message.to.join(",")).into(),
        format!("--subject={}", message.subject).into(),
        format!("--body={}", message.body).into(),
    ];
    if let Some(path) = message.attachment {
        // Always an absolute path under the library, so it cannot start
        // with `-`; an `OsString` so a non-UTF-8 library path survives.
        argv.push("-a".into());
        argv.push(path.as_os_str().to_owned());
    }
    argv.push("--format".into());
    argv.push("json".into());
    argv
}

/// The argument vector for the preflight's profile lookup.
#[must_use]
pub fn profile_argv() -> Vec<OsString> {
    [
        "gmail",
        "users",
        "getProfile",
        "--params",
        r#"{"userId":"me"}"#,
        "--format",
        "json",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

/// The code for a failure that is not a recognised rejection.
///
/// The exit status decides when it is one `gws` documents; otherwise the
/// `error[<kind>]` tag on stderr does. The rest of stderr is never looked at.
fn failure_code(status: Option<i32>, stderr: &[u8]) -> &'static str {
    match status {
        None => "exit_signal",
        Some(1) => "exit_api",
        Some(2) => "exit_auth",
        Some(3) => "exit_validation",
        Some(4) => "exit_discovery",
        Some(5) => "exit_internal",
        Some(_) => {
            let tag = |kind: &str| {
                let needle = format!("error[{kind}]");
                stderr.windows(needle.len()).any(|w| w == needle.as_bytes())
            };
            if tag("api") {
                "exit_other_api"
            } else if tag("auth") {
                "exit_other_auth"
            } else {
                "exit_other"
            }
        }
    }
}

/// The HTTP status of a `{"error": {"code": N}}` answer, if that is what
/// `stdout` is.
fn error_http_status(stdout: &[u8]) -> Option<u16> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let code = value.get("error")?.get("code")?.as_u64()?;
    u16::try_from(code).ok()
}

/// A message id worth storing: short and made of id characters only.
fn plausible_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Map a finished send to its outcome.
///
/// `status` is `None` when a signal ended the process. stderr is used only
/// to pick a code when the exit status is not one `gws` documents.
///
/// # Errors
///
/// [`MailError::Refused`] or [`MailError::Other`]; never `SpawnFailed` or
/// `Timeout`, which are decided before there is an exit status.
pub fn classify_send(
    status: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<String>, MailError> {
    match status {
        Some(0) => Ok(serde_json::from_slice::<serde_json::Value>(stdout)
            .ok()
            .and_then(|v| v.get("id")?.as_str().map(str::to_owned))
            .filter(|id| plausible_id(id))),
        // OT1: Google's rejection of the request is exit 1 with the HTTP
        // status in `error.code`. Only a 4xx is a rejection; a 5xx may have
        // been accepted before it failed, so it stays unknown.
        Some(1) => match error_http_status(stdout) {
            Some(http_status @ 400..=499) => Err(MailError::Refused { http_status }),
            _ => Err(MailError::Other {
                code: failure_code(status, stderr),
            }),
        },
        _ => Err(MailError::Other {
            code: failure_code(status, stderr),
        }),
    }
}

/// Map a finished `getProfile` to the account's lowercase address.
///
/// # Errors
///
/// [`MailError::Other`] for any failure or an unreadable answer.
pub fn classify_profile(
    status: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<String, MailError> {
    if status != Some(0) {
        return Err(MailError::Other {
            code: failure_code(status, stderr),
        });
    }
    let address = serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| v.get("emailAddress")?.as_str().map(str::to_ascii_lowercase))
        .ok_or(MailError::Other {
            code: "profile_unreadable",
        })?;
    match address.rsplit_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() => Ok(address),
        _ => Err(MailError::Other {
            code: "profile_unreadable",
        }),
    }
}

/// The `gws` binary: `configured` when set (and only it), else `PATH` first,
/// then Homebrew's and the pkg installer's locations, as `resolve_gh` does.
#[must_use]
pub fn resolve_gws(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured.filter(|p| !p.as_os_str().is_empty()) {
        // The person named this binary; finding a different one on PATH
        // would send through an install they did not choose.
        return path.is_file().then(|| path.to_path_buf());
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("gws")).collect())
        .unwrap_or_default();
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/gws"),
        PathBuf::from("/usr/local/bin/gws"),
    ]);
    candidates.into_iter().find(|p| p.is_file())
}

/// What a finished child left behind.
#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) status: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Run `program` in its own process group with `profile_dir` as the `gws`
/// config dir; kill the group at `timeout`.
pub(crate) fn run_group(
    program: &Path,
    args: &[OsString],
    profile_dir: &Path,
    timeout: Duration,
) -> Result<Captured, MailError> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let mut command = Command::new(program);
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|k| k.starts_with("GOOGLE_WORKSPACE_") && k != KEYRING_BACKEND_ENV)
        {
            command.env_remove(key);
        }
    }
    let mut child = command
        .args(args)
        .env(CONFIG_DIR_ENV, profile_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|_| MailError::SpawnFailed)?;
    // The child leads its own group, so its pid is the group id.
    let group = libc::pid_t::try_from(child.id()).unwrap_or(0);
    let kill_group = || {
        if group > 0 {
            // SAFETY: a negative pid signals the process group. Before the
            // child is reaped its pid cannot be reused; after, the group id
            // stays taken while any member (the grandchild we are after)
            // lives.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    };

    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_group();
                let _ = child.wait();
                return Err(MailError::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                kill_group();
                let _ = child.wait();
                return Err(MailError::Other {
                    code: "wait_failed",
                });
            }
        }
    };
    // A grandchild still holding a pipe would keep a reader blocked; give
    // the readers until the deadline (and at least a second), then kill the
    // group.
    let grace = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_secs(1));
    let collect = |rx: &mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(grace).or_else(|_| {
            kill_group();
            rx.recv_timeout(Duration::from_secs(1))
        })
    };
    let stdout = collect(&stdout).unwrap_or_default();
    let stderr = collect(&stderr).unwrap_or_default();
    Ok(Captured {
        status: status.code(),
        stdout,
        stderr,
    })
}

/// Read a pipe to its end on a thread, so a full stderr cannot stall stdout.
fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = tx.send(bytes);
    });
    rx
}

/// The real `gws`.
#[derive(Debug, Clone, Copy)]
pub struct SystemGws {
    timeout: Duration,
}

impl Default for SystemGws {
    fn default() -> Self {
        Self {
            timeout: MAIL_TIMEOUT,
        }
    }
}

impl SystemGws {
    fn run(&self, config: &MailConfig, args: &[OsString]) -> Result<Captured, MailError> {
        let program = resolve_gws(config.binary.as_deref()).ok_or(MailError::SpawnFailed)?;
        run_group(&program, args, &config.profile_dir, self.timeout)
    }
}

impl MailRunner for SystemGws {
    fn send(
        &self,
        permit: SendPermit,
        config: &MailConfig,
        message: &MailMessage<'_>,
    ) -> Result<Option<String>, MailError> {
        let _: SendPermit = permit;
        let out = self.run(config, &send_argv(message))?;
        classify_send(out.status, &out.stdout, &out.stderr)
    }

    fn profile_address(&self, config: &MailConfig) -> Result<String, MailError> {
        let out = self.run(config, &profile_argv())?;
        classify_profile(out.status, &out.stdout, &out.stderr)
    }
}

/// Why a preflight failed. Every one pauses sending as `gws_unavailable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightError {
    /// `gws` did not start, or `getProfile` failed.
    Gws(MailError),
    /// The profile's address is not the stored verified sender.
    SenderChanged,
    /// The profile's address is outside `allowed_domains`.
    SenderDomainNotAllowed,
}

impl PreflightError {
    /// The code a log line or settings error carries.
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            Self::Gws(e) => format!("gws_unavailable:{}", e.code()),
            Self::SenderChanged => "sender_changed".to_owned(),
            Self::SenderDomainNotAllowed => "sender_domain_not_allowed".to_owned(),
        }
    }
}

/// Proof that a preflight passed: the only way to get a sender to send from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflighted {
    sender: String,
    config: MailConfig,
}

impl Preflighted {
    /// The verified sender.
    #[must_use]
    pub fn sender(&self) -> &str {
        &self.sender
    }

    /// The profile and binary the preflight ran against.
    #[must_use]
    pub fn config(&self) -> &MailConfig {
        &self.config
    }

    #[cfg(test)]
    pub(crate) fn for_test(sender: &str, config: MailConfig) -> Self {
        Self {
            sender: sender.to_owned(),
            config,
        }
    }
}

/// What a preflight checks against.
#[derive(Debug, Clone, Copy)]
pub struct PreflightRequest<'a> {
    /// The profile and binary.
    pub config: &'a MailConfig,
    /// `allowed_domains`.
    pub allowed_domains: &'a [String],
    /// The stored verified sender; `None` on the settings save that stores it.
    pub verified_sender: Option<&'a str>,
}

/// The preflight, with its 10-minute memory of the last pass.
///
/// Only a pass is remembered: a failure re-runs on every call, which is what
/// "re-checked every pass while paused" needs. The memory is keyed by the
/// whole request, so changing the profile, binary, domains or stored sender
/// is a fresh check.
#[derive(Debug, Default)]
pub struct Preflight {
    last_pass: Option<(i64, CacheKey, Preflighted)>,
}

type CacheKey = (MailConfig, Vec<String>, Option<String>);

impl Preflight {
    /// The send loop's check: a pass younger than 10 minutes is reused.
    ///
    /// # Errors
    ///
    /// [`PreflightError`].
    pub fn check(
        &mut self,
        runner: &dyn MailRunner,
        request: &PreflightRequest<'_>,
        now_ms: i64,
    ) -> Result<Preflighted, PreflightError> {
        let key: CacheKey = (
            request.config.clone(),
            request.allowed_domains.to_vec(),
            request.verified_sender.map(str::to_owned),
        );
        if let Some((at_ms, cached_key, pass)) = &self.last_pass
            && *cached_key == key
            && now_ms >= *at_ms
            && now_ms - *at_ms < PREFLIGHT_TTL_MS
        {
            return Ok(pass.clone());
        }
        self.last_pass = None;
        let sender = runner
            .profile_address(request.config)
            .map_err(PreflightError::Gws)?;
        if let Some(verified) = request.verified_sender
            && !verified.eq_ignore_ascii_case(&sender)
        {
            return Err(PreflightError::SenderChanged);
        }
        if !address_allowed(&sender, request.allowed_domains) {
            return Err(PreflightError::SenderDomainNotAllowed);
        }
        let pass = Preflighted {
            sender,
            config: request.config.clone(),
        };
        self.last_pass = Some((now_ms, key, pass.clone()));
        Ok(pass)
    }

    /// The settings save's check: always runs `gws`.
    ///
    /// # Errors
    ///
    /// [`PreflightError`].
    pub fn check_fresh(
        &mut self,
        runner: &dyn MailRunner,
        request: &PreflightRequest<'_>,
        now_ms: i64,
    ) -> Result<Preflighted, PreflightError> {
        self.last_pass = None;
        self.check(runner, request, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_send_argv_uses_equals_forms_so_leading_dashes_stay_values() {
        let to = vec!["a@example.com".to_owned(), "b@example.com".to_owned()];
        let attachment = PathBuf::from("/lib/release-tmp/m1/2026-01-02-sync.md");
        let message = MailMessage {
            from: "owner@example.com",
            to: &to,
            subject: "--to=evil@example.org",
            body: "-a /etc/passwd",
            attachment: Some(&attachment),
        };
        assert_eq!(
            send_argv(&message),
            os(&[
                "gmail",
                "+send",
                "--from=owner@example.com",
                "--to=a@example.com,b@example.com",
                "--subject=--to=evil@example.org",
                "--body=-a /etc/passwd",
                "-a",
                "/lib/release-tmp/m1/2026-01-02-sync.md",
                "--format",
                "json",
            ])
        );
    }

    #[test]
    fn the_notice_argv_has_no_attachment_and_an_empty_body() {
        let to = vec!["owner@example.com".to_owned()];
        let message = MailMessage {
            from: "owner@example.com",
            to: &to,
            subject: "s",
            body: "",
            attachment: None,
        };
        let argv = send_argv(&message);
        assert!(argv.contains(&OsString::from("--body=")), "{argv:?}");
        assert!(!argv.contains(&OsString::from("-a")), "{argv:?}");
    }

    #[test]
    fn the_profile_argv_is_get_profile_for_me_as_json() {
        assert_eq!(
            profile_argv(),
            os(&[
                "gmail",
                "users",
                "getProfile",
                "--params",
                r#"{"userId":"me"}"#,
                "--format",
                "json",
            ])
        );
    }

    const REJECTION: &str = r#"{
  "error": {
    "code": 403,
    "message": "Request had insufficient authentication scopes.",
    "reason": "insufficientPermissions"
  }
}"#;

    #[test]
    fn exit_zero_is_sent_with_the_message_id_when_present() {
        assert_eq!(
            classify_send(Some(0), br#"{"id":"18c0ffee","threadId":"18c0ffee"}"#, b""),
            Ok(Some("18c0ffee".to_owned()))
        );
        assert_eq!(classify_send(Some(0), b"not json", b""), Ok(None));
        assert_eq!(
            classify_send(Some(0), br#"{"id":"has space"}"#, b""),
            Ok(None),
            "an id that is not an id is not stored"
        );
    }

    #[test]
    fn an_api_4xx_on_exit_one_is_refused() {
        assert_eq!(
            classify_send(Some(1), REJECTION.as_bytes(), b"error[api]: anything"),
            Err(MailError::Refused { http_status: 403 })
        );
    }

    #[test]
    fn every_other_failure_is_an_unknown_outcome_carrying_only_a_code() {
        let five_hundred = REJECTION.replace("403", "503");
        let cases: [(Option<i32>, &[u8], &str); 7] = [
            (Some(1), five_hundred.as_bytes(), "exit_api"),
            (Some(1), b"garbage", "exit_api"),
            (Some(2), REJECTION.as_bytes(), "exit_auth"),
            (Some(3), b"", "exit_validation"),
            (Some(127), b"", "exit_other_api"),
            (None, b"", "exit_signal"),
            (Some(5), b"", "exit_internal"),
        ];
        for (status, stdout, code) in cases {
            assert_eq!(
                classify_send(status, stdout, b"error[api]: secret@example.com said hi"),
                Err(MailError::Other { code }),
                "{status:?}"
            );
        }
        assert_eq!(
            classify_send(Some(127), b"", b"env: node: No such file or directory"),
            Err(MailError::Other { code: "exit_other" }),
            "an npm install without node is a started process, never a spawn failure"
        );
    }

    #[test]
    fn get_profile_yields_the_lowercase_email_address() {
        assert_eq!(
            classify_profile(
                Some(0),
                br#"{"emailAddress":"Owner@Example.com","messagesTotal":1}"#,
                b""
            ),
            Ok("owner@example.com".to_owned())
        );
        assert!(classify_profile(Some(0), br#"{"emailAddress":"nope"}"#, b"").is_err());
        assert!(classify_profile(Some(2), REJECTION.as_bytes(), b"").is_err());
    }

    #[test]
    fn mail_error_codes_carry_no_text() {
        for e in [
            MailError::SpawnFailed,
            MailError::Refused { http_status: 403 },
            MailError::Timeout,
            MailError::Other { code: "exit_auth" },
        ] {
            let s = format!("{e} {e:?}");
            assert!(!s.contains('@') && !s.contains("insufficient"), "{s}");
        }
    }

    #[test]
    fn a_configured_binary_wins_and_is_never_second_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let gws = dir.path().join("gws");
        std::fs::write(&gws, b"").unwrap();
        assert_eq!(resolve_gws(Some(&gws)), Some(gws.clone()));
        assert_eq!(
            resolve_gws(Some(&dir.path().join("missing"))),
            None,
            "a configured path that is not there does not fall back to PATH"
        );
    }

    fn sh(script: &str) -> Vec<OsString> {
        vec![OsString::from("-c"), OsString::from(script)]
    }

    #[test]
    fn the_child_sees_the_profile_dir_and_its_output_comes_back() {
        let out = run_group(
            Path::new("/bin/sh"),
            &sh("printf %s \"$GOOGLE_WORKSPACE_CLI_CONFIG_DIR\"; printf e >&2; exit 4"),
            Path::new("/profile/example"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(out.status, Some(4));
        assert_eq!(out.stdout, b"/profile/example");
        assert_eq!(out.stderr, b"e");
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure_and_a_started_one_never_is() {
        assert_eq!(
            run_group(
                Path::new("/nonexistent/gws"),
                &[],
                Path::new("/p"),
                Duration::from_secs(5)
            )
            .unwrap_err(),
            MailError::SpawnFailed
        );
        // The npm case: the script starts and its interpreter is missing.
        let out = run_group(
            Path::new("/bin/sh"),
            &sh("exit 127"),
            Path::new("/p"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.status, Some(127));
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pidfile.to_str().unwrap());
        let started = Instant::now();
        let result = run_group(
            Path::new("/bin/sh"),
            &sh(&script),
            Path::new("/p"),
            Duration::from_millis(500),
        );
        assert_eq!(result.unwrap_err(), MailError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(10));
        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(grandchild) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(grandchild), "the grandchild outlived the timeout");
    }

    /// A runner whose profile answer is scripted and whose calls are counted.
    struct ProfileRunner {
        answer: Mutex<Result<String, MailError>>,
        calls: Mutex<u32>,
    }

    impl ProfileRunner {
        fn new(answer: Result<String, MailError>) -> Self {
            Self {
                answer: Mutex::new(answer),
                calls: Mutex::new(0),
            }
        }
        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl MailRunner for ProfileRunner {
        fn send(
            &self,
            _: SendPermit,
            _: &MailConfig,
            _: &MailMessage<'_>,
        ) -> Result<Option<String>, MailError> {
            panic!("a preflight never sends")
        }
        fn profile_address(&self, _: &MailConfig) -> Result<String, MailError> {
            *self.calls.lock().unwrap() += 1;
            self.answer.lock().unwrap().clone()
        }
    }

    fn domains() -> Vec<String> {
        vec!["example.com".to_owned()]
    }

    #[test]
    fn preflight_rejects_a_sender_whose_domain_is_not_allowed() {
        let config = MailConfig::default();
        let allowed = domains();
        let runner = ProfileRunner::new(Ok("owner@example.org".to_owned()));
        let request = PreflightRequest {
            config: &config,
            allowed_domains: &allowed,
            verified_sender: None,
        };
        assert_eq!(
            Preflight::default().check(&runner, &request, 0),
            Err(PreflightError::SenderDomainNotAllowed)
        );
        let sub = ProfileRunner::new(Ok("owner@mail.example.com".to_owned()));
        assert_eq!(
            Preflight::default().check(&sub, &request, 0),
            Err(PreflightError::SenderDomainNotAllowed),
            "subdomains do not match"
        );
    }

    #[test]
    fn preflight_rejects_a_sender_that_is_not_the_verified_one() {
        let config = MailConfig::default();
        let allowed = domains();
        let runner = ProfileRunner::new(Ok("other@example.com".to_owned()));
        let request = PreflightRequest {
            config: &config,
            allowed_domains: &allowed,
            verified_sender: Some("owner@example.com"),
        };
        assert_eq!(
            Preflight::default().check(&runner, &request, 0),
            Err(PreflightError::SenderChanged)
        );
    }

    #[test]
    fn preflight_failures_of_gws_pause_as_unavailable() {
        let config = MailConfig::default();
        let allowed = domains();
        let runner = ProfileRunner::new(Err(MailError::SpawnFailed));
        let request = PreflightRequest {
            config: &config,
            allowed_domains: &allowed,
            verified_sender: None,
        };
        let err = Preflight::default()
            .check(&runner, &request, 0)
            .unwrap_err();
        assert_eq!(err, PreflightError::Gws(MailError::SpawnFailed));
        assert!(err.code().starts_with("gws_unavailable"));
    }

    #[test]
    fn a_pass_is_cached_for_ten_minutes_and_a_failure_is_not() {
        let config = MailConfig::default();
        let allowed = domains();
        let runner = ProfileRunner::new(Ok("owner@example.com".to_owned()));
        let request = PreflightRequest {
            config: &config,
            allowed_domains: &allowed,
            verified_sender: Some("owner@example.com"),
        };
        let mut preflight = Preflight::default();
        let pass = preflight.check(&runner, &request, 1_000).unwrap();
        assert_eq!(pass.sender(), "owner@example.com");
        preflight
            .check(&runner, &request, 1_000 + PREFLIGHT_TTL_MS - 1)
            .unwrap();
        assert_eq!(runner.calls(), 1, "reused inside ten minutes");
        preflight
            .check(&runner, &request, 1_000 + PREFLIGHT_TTL_MS)
            .unwrap();
        assert_eq!(runner.calls(), 2, "re-run at ten minutes");

        let other_domains = vec!["example.org".to_owned()];
        let changed = PreflightRequest {
            allowed_domains: &other_domains,
            ..request
        };
        assert!(
            preflight
                .check(&runner, &changed, 1_000 + PREFLIGHT_TTL_MS)
                .is_err()
        );
        assert_eq!(runner.calls(), 3, "a changed request is a fresh check");

        *runner.answer.lock().unwrap() = Err(MailError::Timeout);
        let mut failing = Preflight::default();
        assert!(failing.check(&runner, &request, 0).is_err());
        assert!(failing.check(&runner, &request, 1).is_err());
        assert_eq!(runner.calls(), 5, "a failure is re-checked every pass");

        *runner.answer.lock().unwrap() = Ok("owner@example.com".to_owned());
        preflight.check(&runner, &request, 2_000).unwrap();
        let before = runner.calls();
        preflight.check_fresh(&runner, &request, 2_001).unwrap();
        assert_eq!(
            runner.calls(),
            before + 1,
            "a settings save always runs gws"
        );
    }
}
