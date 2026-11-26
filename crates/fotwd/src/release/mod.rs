//! Attendee release (#326): emailing one meeting's reviewed copy to the
//! invitees in an allowed internal domain.
//!
//! This module holds the send side (#328): the two chokepoints that may put
//! mail on the wire, [`send_one`] and [`send_notice`], the attachment temp
//! files, and the transport in [`mail`]. The release table, the worker and its
//! settings arrive in #331, which implements [`SendStore`] and
//! [`NoticeStore`] on the table and drives these functions from the send loop.
//!
//! # At most once (D13)
//!
//! A duplicate email is recoverable; an automatic resend loop of confidential
//! content is not. So the row is moved `scheduled → sending` and committed
//! *before* `gws` is spawned, and the only outcome that puts it back to
//! `scheduled` is one where no process ran. Every outcome that is not a clear
//! success or a recognised rejection becomes `needs_review(send_outcome_unknown)`,
//! which a person has to resolve.
//!
//! # Codes, not content (D15)
//!
//! The log lines here carry a meeting id, a state and a reason code. Nothing in
//! this module formats an address, subject, body, title or `gws` output into a
//! line, an error or a notice.

pub mod mail;
pub mod types;

use std::path::{Path, PathBuf};

pub use mail::{
    MailConfig, MailError, MailMessage, MailRunner, Preflight, PreflightError, PreflightRequest,
    Preflighted, SystemGws,
};

/// The directory under the library that holds attachment temp files.
pub const TEMP_DIR: &str = "release-tmp";

/// How long the owner notice waits after the previous one (D16).
pub const NOTICE_INTERVAL_MS: i64 = 60 * 60 * 1_000;

/// The right to call [`MailRunner::send`] once.
///
/// Minted only inside [`send_one`] and [`send_notice`] (a test scans the crate
/// to keep it that way), and consumed by the call, so no other code path can
/// put mail on the wire.
#[derive(Debug)]
pub struct SendPermit(());

impl SendPermit {
    fn mint() -> Self {
        Self(())
    }
}

/// Whether `address`'s domain exactly equals an entry of `allowed_domains`.
///
/// Case-insensitive; subdomains do not match (D3).
#[must_use]
pub fn address_allowed(address: &str, allowed_domains: &[String]) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && allowed_domains
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(domain))
}

/// A meeting id safe to use as one path component.
fn safe_meeting_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `<YYYY-MM-DD>-<slug>.md` with D9's slug alphabet, at most 60 characters.
fn safe_attachment_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    let bytes = stem.as_bytes();
    if bytes.len() < 12 {
        return false;
    }
    let (date, rest) = bytes.split_at(10);
    let date_ok = date.iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        _ => b.is_ascii_digit(),
    });
    let slug = &rest[1..];
    date_ok
        && rest[0] == b'-'
        && !slug.is_empty()
        && slug.len() <= 60
        && slug
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// A release row as the send needs it: the frozen, rendered snapshot.
///
/// #331 builds this from a `meeting_releases` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRow {
    /// The meeting id (the row's key). Used in the temp path and logs.
    pub meeting_id: String,
    /// The frozen recipient addresses (D4).
    pub recipients: Vec<String>,
    /// `rendered_subject`.
    pub subject: String,
    /// `rendered_body`.
    pub body: String,
    /// `<YYYY-MM-DD>-<slug>.md` (D9), the name `gws` gives the attachment.
    pub attachment_name: String,
    /// `rendered_attachment`.
    pub attachment: Vec<u8>,
}

/// A store failure. Carries nothing: the store logs its own code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreError;

/// The release table, as [`send_one`] drives it.
///
/// Every method is a compare-and-set on the row's current state, committed
/// before it returns; `Ok(false)` means the row was not in the expected state
/// (a Cancel won, say) and nothing changed.
pub trait SendStore {
    /// `scheduled → sending`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the write failed.
    fn claim_sending(&self, meeting_id: &str) -> Result<bool, StoreError>;

    /// Apply `outcome`: from `scheduled` for [`SendOutcome::NoRecipients`],
    /// from `sending` for every other outcome except
    /// [`SendOutcome::LostRace`], which is never settled.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the write failed.
    fn settle(&self, meeting_id: &str, outcome: &SendOutcome) -> Result<bool, StoreError>;
}

/// What [`send_one`] did to a row (D13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// Every recipient was filtered out → `dropped(no_recipients)`.
    NoRecipients,
    /// The row was not `scheduled` any more; nothing happened.
    LostRace,
    /// Exit 0 → `sent`.
    Sent {
        /// The addresses actually used, after the re-filter.
        sent_to: Vec<String>,
        /// The message id, when `gws` printed one.
        message_id: Option<String>,
    },
    /// No process ran → back to `scheduled`; the worker pauses
    /// `gws_unavailable`.
    GwsUnavailable,
    /// A recognised API rejection → `held(send_refused)`.
    SendRefused {
        /// The rejection's HTTP status.
        http_status: u16,
    },
    /// Anything else → `needs_review(send_outcome_unknown)`.
    OutcomeUnknown {
        /// The classified error.
        error: MailError,
    },
}

impl SendOutcome {
    /// The state the row ends in.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::NoRecipients => "dropped",
            Self::LostRace | Self::GwsUnavailable => "scheduled",
            Self::Sent { .. } => "sent",
            Self::SendRefused { .. } => "held",
            Self::OutcomeUnknown { .. } => "needs_review",
        }
    }

    /// The D10 reason code, when the state has one.
    #[must_use]
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::NoRecipients => Some("no_recipients"),
            Self::GwsUnavailable => Some("gws_unavailable"),
            Self::SendRefused { .. } => Some("send_refused"),
            Self::OutcomeUnknown { .. } => Some("send_outcome_unknown"),
            Self::LostRace | Self::Sent { .. } => None,
        }
    }

    /// Whether the worker must pause sending (`gws_unavailable`).
    #[must_use]
    pub fn pauses_worker(&self) -> bool {
        matches!(self, Self::GwsUnavailable)
    }
}

/// Why [`send_one`] stopped before deciding an outcome. The row is unchanged
/// unless the variant says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The meeting id or attachment name is not safe to use as a path.
    InvalidRow,
    /// The preflight's sender is outside the current `allowed_domains`.
    SenderNotAllowed,
    /// The attachment temp file could not be written.
    TempFile,
    /// A store write failed. If it was the final settle, the row is left in
    /// `sending`, which #331's startup check turns into
    /// `needs_review(send_outcome_unknown)`.
    Store,
}

impl SendError {
    /// The code this error is logged as.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRow => "invalid_row",
            Self::SenderNotAllowed => "sender_not_allowed",
            Self::TempFile => "temp_file_failed",
            Self::Store => "store_failed",
        }
    }
}

/// The only path that sends a release (D13).
///
/// In order: re-filter the frozen recipients against `allowed_domains` (none
/// left → `dropped(no_recipients)`); write the attachment temp file;
/// compare-and-set `scheduled → sending`; run `gws`; remove the temp file;
/// settle the outcome.
///
/// # Errors
///
/// [`SendError`]; see its variants for what state the row is in.
pub fn send_one(
    runner: &dyn MailRunner,
    store: &dyn SendStore,
    preflight: &Preflighted,
    library: &Path,
    allowed_domains: &[String],
    row: &SendRow,
) -> Result<SendOutcome, SendError> {
    if !safe_meeting_id(&row.meeting_id) || !safe_attachment_name(&row.attachment_name) {
        return Err(SendError::InvalidRow);
    }

    // 1. Re-filter: the list may only shrink (D4). De-duplicated, order kept.
    let mut sent_to: Vec<String> = Vec::new();
    for address in &row.recipients {
        let address = address.to_ascii_lowercase();
        if address_allowed(&address, allowed_domains) && !sent_to.contains(&address) {
            sent_to.push(address);
        }
    }
    if sent_to.is_empty() {
        return settle(store, &row.meeting_id, SendOutcome::NoRecipients);
    }
    if !address_allowed(preflight.sender(), allowed_domains) {
        return Err(SendError::SenderNotAllowed);
    }

    // The temp file is local and harmless, so it is written before the
    // claim: a write failure then leaves the row untouched.
    let temp = write_attachment(
        library,
        &row.meeting_id,
        &row.attachment_name,
        &row.attachment,
    )
    .map_err(|_| SendError::TempFile)?;

    // 2. Compare-and-set `scheduled → sending`, committed before the spawn.
    if !store
        .claim_sending(&row.meeting_id)
        .map_err(|_| SendError::Store)?
    {
        return Ok(SendOutcome::LostRace);
    }

    // 3. Run `gws` once.
    let message = MailMessage {
        from: preflight.sender(),
        to: &sent_to,
        subject: &row.subject,
        body: &row.body,
        attachment: Some(temp.path()),
    };
    let result = MailRunner::send(runner, SendPermit::mint(), preflight.config(), &message);
    drop(temp);

    let outcome = match result {
        Ok(message_id) => SendOutcome::Sent {
            sent_to,
            message_id,
        },
        Err(MailError::SpawnFailed) => SendOutcome::GwsUnavailable,
        Err(MailError::Refused { http_status }) => SendOutcome::SendRefused { http_status },
        Err(error) => SendOutcome::OutcomeUnknown { error },
    };
    settle(store, &row.meeting_id, outcome)
}

/// Settle `outcome` and log it.
///
/// A lost compare-and-set before the claim ([`SendOutcome::NoRecipients`])
/// means nothing happened, so it becomes [`SendOutcome::LostRace`]. After the
/// claim, `gws` has run and its outcome is what happened whatever the table
/// says (the meeting may have been deleted mid-send), so it is returned as is.
fn settle(
    store: &dyn SendStore,
    meeting_id: &str,
    outcome: SendOutcome,
) -> Result<SendOutcome, SendError> {
    let applied = match store.settle(meeting_id, &outcome) {
        Ok(applied) => applied,
        Err(StoreError) => {
            crate::diag!(
                "  ! release: meeting {meeting_id} -> {} not stored ({})",
                outcome.state(),
                SendError::Store.code()
            );
            return Err(SendError::Store);
        }
    };
    if !applied {
        if outcome == SendOutcome::NoRecipients {
            return Ok(SendOutcome::LostRace);
        }
        crate::diag!(
            "  ! release: meeting {meeting_id} left sending before its outcome was stored"
        );
    }
    crate::diag!("{}", outcome_line(meeting_id, &outcome));
    Ok(outcome)
}

/// The log line for a send: meeting id, state and code only.
#[must_use]
pub fn outcome_line(meeting_id: &str, outcome: &SendOutcome) -> String {
    let detail = match outcome {
        SendOutcome::SendRefused { http_status } => format!(" http_{http_status}"),
        SendOutcome::OutcomeUnknown { error } => format!(" {}", error.code()),
        SendOutcome::Sent { sent_to, .. } => format!(" to {} recipient(s)", sent_to.len()),
        _ => String::new(),
    };
    let reason = outcome
        .reason()
        .map(|r| format!(" ({r})"))
        .unwrap_or_default();
    format!(
        "  release: meeting {meeting_id} -> {}{reason}{detail}",
        outcome.state()
    )
}

/// The owner-notice side of the release table (D16).
pub trait NoticeStore {
    /// How many rows are in `needs_review` or `held` (the Outbox badge).
    ///
    /// # Errors
    ///
    /// [`StoreError`].
    fn waiting_count(&self) -> Result<usize, StoreError>;

    /// When a row last entered `needs_review` or `held`, epoch ms.
    ///
    /// # Errors
    ///
    /// [`StoreError`].
    fn last_attention_ms(&self) -> Result<Option<i64>, StoreError>;

    /// `release_notice_last_ms`.
    ///
    /// # Errors
    ///
    /// [`StoreError`].
    fn last_notice_ms(&self) -> Result<Option<i64>, StoreError>;

    /// Write `release_notice_last_ms`; called only after a successful send.
    ///
    /// # Errors
    ///
    /// [`StoreError`].
    fn record_notice(&self, at_ms: i64) -> Result<(), StoreError>;
}

/// What [`send_notice`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeOutcome {
    /// No `notify_address` set.
    NoAddress,
    /// `notify_address` is outside `allowed_domains`.
    AddressNotAllowed,
    /// Nothing new since the last notice, or it was less than an hour ago.
    NotDue,
    /// Sent; `release_notice_last_ms` recorded.
    Sent {
        /// The count in the subject.
        waiting: usize,
    },
    /// The send or a store read failed; retried on the next qualifying pass.
    Failed {
        /// The code.
        code: String,
    },
}

/// The notice's subject: a count and nothing else.
#[must_use]
pub fn notice_subject(waiting: usize) -> String {
    let noun = if waiting == 1 { "meeting" } else { "meetings" };
    format!("FlyOnTheWall: {waiting} {noun} waiting for release review")
}

/// The only path that sends the owner notice (D16): content-free, at most
/// hourly, and only when a row entered review or held since the last one.
pub fn send_notice(
    runner: &dyn MailRunner,
    store: &dyn NoticeStore,
    preflight: &Preflighted,
    allowed_domains: &[String],
    notify_address: Option<&str>,
    now_ms: i64,
) -> NoticeOutcome {
    let Some(address) = notify_address
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_ascii_lowercase)
    else {
        return NoticeOutcome::NoAddress;
    };
    if !address_allowed(&address, allowed_domains)
        || !address_allowed(preflight.sender(), allowed_domains)
    {
        return NoticeOutcome::AddressNotAllowed;
    }
    let read = || -> Result<(usize, Option<i64>, Option<i64>), StoreError> {
        Ok((
            store.waiting_count()?,
            store.last_attention_ms()?,
            store.last_notice_ms()?,
        ))
    };
    let Ok((waiting, attention, last)) = read() else {
        return failed_notice("store_failed");
    };
    let something_new = attention.is_some_and(|a| last.is_none_or(|l| a > l));
    let hour_passed = last.is_none_or(|l| now_ms.saturating_sub(l) >= NOTICE_INTERVAL_MS);
    if waiting == 0 || !something_new || !hour_passed {
        return NoticeOutcome::NotDue;
    }

    let to = [address];
    let subject = notice_subject(waiting);
    let message = MailMessage {
        from: preflight.sender(),
        to: &to,
        subject: &subject,
        body: "",
        attachment: None,
    };
    match MailRunner::send(runner, SendPermit::mint(), preflight.config(), &message) {
        Ok(_) => {
            if store.record_notice(now_ms).is_err() {
                return failed_notice("store_failed");
            }
            crate::diag!("  release: owner notice sent ({waiting} waiting)");
            NoticeOutcome::Sent { waiting }
        }
        Err(error) => failed_notice(&error.code()),
    }
}

fn failed_notice(code: &str) -> NoticeOutcome {
    crate::diag!("  ! release: owner notice not sent ({code})");
    NoticeOutcome::Failed {
        code: code.to_owned(),
    }
}

/// An attachment temp file, removed (with its meeting directory) on drop.
#[derive(Debug)]
pub(crate) struct TempAttachment {
    dir: PathBuf,
    path: PathBuf,
}

impl TempAttachment {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempAttachment {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Write `bytes` to `<library>/release-tmp/<meeting_id>/<name>`, directories
/// 0700 and file 0600.
pub(crate) fn write_attachment(
    library: &Path,
    meeting_id: &str,
    name: &str,
    bytes: &[u8],
) -> std::io::Result<TempAttachment> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let root = library.join(TEMP_DIR);
    let dir = root.join(meeting_id);
    for d in [&root, &dir] {
        match std::fs::DirBuilder::new().mode(0o700).create(d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Only a real directory of ours, never a symlink elsewhere.
                if !std::fs::symlink_metadata(d)?.is_dir() {
                    return Err(std::io::Error::other("release-tmp is not a directory"));
                }
            }
            Err(e) => return Err(e),
        }
        // `mode` is filtered by the umask; set it outright.
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(name);
    // A leftover from a crashed send is replaced, never appended to.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let temp = TempAttachment {
        dir,
        path: path.clone(),
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(temp)
}

/// Remove the whole `release-tmp` directory; run at daemon start.
///
/// # Errors
///
/// Any I/O error other than the directory being absent.
pub fn sweep_temp(library: &Path) -> std::io::Result<()> {
    let root = library.join(TEMP_DIR);
    let result = match std::fs::symlink_metadata(&root) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&root),
        // A symlink or stray file is removed itself, never followed.
        Ok(_) => std::fs::remove_file(&root),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Mutex;

    // ---------- fakes ----------

    /// A scripted `gws`: records each send and answers from a queue.
    #[derive(Default)]
    struct FakeMail {
        answers: Mutex<Vec<Result<Option<String>, MailError>>>,
        sent: Mutex<Vec<Sent>>,
    }

    #[derive(Debug, Clone)]
    struct Sent {
        from: String,
        to: Vec<String>,
        subject: String,
        body: String,
        attachment: Option<PathBuf>,
        attachment_existed: bool,
        attachment_mode: Option<u32>,
        dir_mode: Option<u32>,
        attachment_bytes: Option<Vec<u8>>,
    }

    impl FakeMail {
        fn answering(answer: Result<Option<String>, MailError>) -> Self {
            let fake = Self::default();
            fake.answers.lock().unwrap().push(answer);
            fake
        }
        fn sent(&self) -> Vec<Sent> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl MailRunner for FakeMail {
        fn send(
            &self,
            _permit: SendPermit,
            _config: &MailConfig,
            m: &MailMessage<'_>,
        ) -> Result<Option<String>, MailError> {
            let meta = m.attachment.and_then(|p| std::fs::metadata(p).ok());
            let dir_meta = m
                .attachment
                .and_then(|p| p.parent())
                .and_then(|d| std::fs::metadata(d).ok());
            self.sent.lock().unwrap().push(Sent {
                from: m.from.to_owned(),
                to: m.to.to_vec(),
                subject: m.subject.to_owned(),
                body: m.body.to_owned(),
                attachment: m.attachment.map(Path::to_path_buf),
                attachment_existed: meta.is_some(),
                attachment_mode: meta.map(|x| x.permissions().mode() & 0o777),
                dir_mode: dir_meta.map(|x| x.permissions().mode() & 0o777),
                attachment_bytes: m.attachment.and_then(|p| std::fs::read(p).ok()),
            });
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                Ok(None)
            } else {
                answers.remove(0)
            }
        }
        fn profile_address(&self, _: &MailConfig) -> Result<String, MailError> {
            Ok("owner@example.com".to_owned())
        }
    }

    /// The release table, in memory, with a compare-and-set state.
    struct FakeStore {
        state: Mutex<String>,
        settled: Mutex<Vec<SendOutcome>>,
        /// What the state was at the moment `gws` ran.
        committed_before_send: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn new(state: &str) -> Self {
            Self {
                state: Mutex::new(state.to_owned()),
                settled: Mutex::new(Vec::new()),
                committed_before_send: Mutex::new(Vec::new()),
            }
        }
        fn state(&self) -> String {
            self.state.lock().unwrap().clone()
        }
    }

    impl SendStore for FakeStore {
        fn claim_sending(&self, _: &str) -> Result<bool, StoreError> {
            let mut s = self.state.lock().unwrap();
            if *s == "scheduled" {
                *s = "sending".to_owned();
                self.committed_before_send.lock().unwrap().push(s.clone());
                Ok(true)
            } else {
                Ok(false)
            }
        }
        fn settle(&self, _: &str, outcome: &SendOutcome) -> Result<bool, StoreError> {
            let mut s = self.state.lock().unwrap();
            let from = if *outcome == SendOutcome::NoRecipients {
                "scheduled"
            } else {
                "sending"
            };
            if *s != from {
                return Ok(false);
            }
            *s = outcome.state().to_owned();
            self.settled.lock().unwrap().push(outcome.clone());
            Ok(true)
        }
    }

    fn allowed() -> Vec<String> {
        vec!["example.com".to_owned()]
    }

    fn pass() -> Preflighted {
        Preflighted::for_test(
            "owner@example.com",
            MailConfig {
                profile_dir: PathBuf::from("/profile/example"),
                binary: None,
            },
        )
    }

    fn row() -> SendRow {
        SendRow {
            meeting_id: "01926f5a-0000-7000-8000-000000000001".to_owned(),
            recipients: vec![
                "alice@example.com".to_owned(),
                "guest@example.org".to_owned(),
                "bob@example.com".to_owned(),
            ],
            subject: "-Notes: Planning sync (2026-01-02)".to_owned(),
            body: "-Summary\n- decided a thing".to_owned(),
            attachment_name: "2026-01-02-planning-sync.md".to_owned(),
            attachment: b"# Notes: Planning sync\n".to_vec(),
        }
    }

    // ---------- send_one outcomes ----------

    #[test]
    fn exit_zero_sends_to_the_refiltered_list_and_records_the_id() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::answering(Ok(Some("18c0ffee".to_owned())));
        let store = FakeStore::new("scheduled");
        let outcome = send_one(&mail, &store, &pass(), lib.path(), &allowed(), &row()).unwrap();
        let expected = SendOutcome::Sent {
            sent_to: vec!["alice@example.com".to_owned(), "bob@example.com".to_owned()],
            message_id: Some("18c0ffee".to_owned()),
        };
        assert_eq!(outcome, expected);
        assert_eq!(store.state(), "sent");
        assert_eq!(
            *store.committed_before_send.lock().unwrap(),
            vec!["sending".to_owned()],
            "the row is committed as sending before gws runs"
        );
        let sent = mail.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].from, "owner@example.com");
        assert_eq!(sent[0].to, vec!["alice@example.com", "bob@example.com"]);
        assert_eq!(sent[0].subject, row().subject);
        assert_eq!(sent[0].body, row().body);
    }

    #[test]
    fn no_recipients_left_drops_the_row_without_running_gws() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::default();
        let store = FakeStore::new("scheduled");
        let mut r = row();
        r.recipients = vec![
            "guest@example.org".to_owned(),
            "x@sub.example.com".to_owned(),
        ];
        let outcome = send_one(&mail, &store, &pass(), lib.path(), &allowed(), &r).unwrap();
        assert_eq!(outcome, SendOutcome::NoRecipients);
        assert_eq!(store.state(), "dropped");
        assert!(mail.sent().is_empty());
    }

    #[test]
    fn a_lost_compare_and_set_sends_nothing_and_changes_nothing() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::default();
        let store = FakeStore::new("needs_review");
        let outcome = send_one(&mail, &store, &pass(), lib.path(), &allowed(), &row()).unwrap();
        assert_eq!(outcome, SendOutcome::LostRace);
        assert_eq!(store.state(), "needs_review");
        assert!(mail.sent().is_empty());
        assert!(!lib.path().join(TEMP_DIR).join(row().meeting_id).exists());
    }

    /// A row deleted while `gws` ran: the claim wins, the settle finds nothing.
    struct VanishingStore;

    impl SendStore for VanishingStore {
        fn claim_sending(&self, _: &str) -> Result<bool, StoreError> {
            Ok(true)
        }
        fn settle(&self, _: &str, _: &SendOutcome) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    #[test]
    fn a_send_whose_row_vanished_still_reports_what_gws_did() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::answering(Ok(None));
        let outcome = send_one(
            &mail,
            &VanishingStore,
            &pass(),
            lib.path(),
            &allowed(),
            &row(),
        )
        .unwrap();
        assert!(
            matches!(outcome, SendOutcome::Sent { .. }),
            "a sent email is never reported as nothing happened: {outcome:?}"
        );
    }

    #[test]
    fn a_spawn_failure_returns_the_row_to_scheduled_and_pauses() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::answering(Err(MailError::SpawnFailed));
        let store = FakeStore::new("scheduled");
        let outcome = send_one(&mail, &store, &pass(), lib.path(), &allowed(), &row()).unwrap();
        assert_eq!(outcome, SendOutcome::GwsUnavailable);
        assert!(outcome.pauses_worker());
        assert_eq!(store.state(), "scheduled");
    }

    #[test]
    fn a_recognised_rejection_holds_the_row() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::answering(Err(MailError::Refused { http_status: 400 }));
        let store = FakeStore::new("scheduled");
        let outcome = send_one(&mail, &store, &pass(), lib.path(), &allowed(), &row()).unwrap();
        assert_eq!(outcome, SendOutcome::SendRefused { http_status: 400 });
        assert_eq!(store.state(), "held");
        assert_eq!(outcome.reason(), Some("send_refused"));
        assert!(!outcome.pauses_worker());
    }

    #[test]
    fn a_timeout_or_unrecognised_error_needs_review_as_outcome_unknown() {
        for error in [
            MailError::Timeout,
            MailError::Other { code: "exit_api" },
            MailError::Other { code: "exit_auth" },
            MailError::Other {
                code: "exit_signal",
            },
        ] {
            let lib = tempfile::tempdir().unwrap();
            let mail = FakeMail::answering(Err(error.clone()));
            let store = FakeStore::new("scheduled");
            let outcome = send_one(&mail, &store, &pass(), lib.path(), &allowed(), &row()).unwrap();
            assert_eq!(
                outcome,
                SendOutcome::OutcomeUnknown {
                    error: error.clone()
                }
            );
            assert_eq!(store.state(), "needs_review", "{error:?}");
            assert_eq!(outcome.reason(), Some("send_outcome_unknown"));
        }
    }

    #[test]
    fn a_sender_outside_the_current_domains_sends_nothing() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::default();
        let store = FakeStore::new("scheduled");
        let only_org = vec!["example.org".to_owned()];
        let mut r = row();
        r.recipients = vec!["guest@example.org".to_owned()];
        assert_eq!(
            send_one(&mail, &store, &pass(), lib.path(), &only_org, &r),
            Err(SendError::SenderNotAllowed)
        );
        assert_eq!(store.state(), "scheduled");
        assert!(mail.sent().is_empty());
    }

    #[test]
    fn a_path_unsafe_row_is_refused_before_any_state_change() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::default();
        for (id, name) in [
            ("../escape", "2026-01-02-sync.md"),
            ("m1", "../../2026-01-02-sync.md"),
            ("m1", "notes.md"),
            ("m1", "2026-01-02-Sync.md"),
            ("", "2026-01-02-sync.md"),
        ] {
            let store = FakeStore::new("scheduled");
            let mut r = row();
            r.meeting_id = id.to_owned();
            r.attachment_name = name.to_owned();
            assert_eq!(
                send_one(&mail, &store, &pass(), lib.path(), &allowed(), &r),
                Err(SendError::InvalidRow),
                "{id} {name}"
            );
            assert_eq!(store.state(), "scheduled");
        }
        assert!(mail.sent().is_empty());
    }

    // ---------- temp files ----------

    #[test]
    fn the_attachment_is_a_private_temp_file_with_the_release_name_removed_after() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::default();
        let store = FakeStore::new("scheduled");
        send_one(&mail, &store, &pass(), lib.path(), &allowed(), &row()).unwrap();
        let sent = &mail.sent()[0];
        let expected = lib
            .path()
            .join("release-tmp")
            .join("01926f5a-0000-7000-8000-000000000001")
            .join("2026-01-02-planning-sync.md");
        assert_eq!(sent.attachment.as_deref(), Some(expected.as_path()));
        assert!(sent.attachment_existed);
        assert_eq!(sent.attachment_mode, Some(0o600));
        assert_eq!(sent.dir_mode, Some(0o700));
        assert_eq!(
            sent.attachment_bytes.as_deref(),
            Some(&row().attachment[..])
        );
        assert!(!expected.exists(), "removed after the call");
        assert!(!expected.parent().unwrap().exists(), "and its directory");
    }

    #[test]
    fn the_temp_file_is_removed_after_a_failed_call_too() {
        let lib = tempfile::tempdir().unwrap();
        let mail = FakeMail::answering(Err(MailError::Timeout));
        let store = FakeStore::new("scheduled");
        send_one(&mail, &store, &pass(), lib.path(), &allowed(), &row()).unwrap();
        assert!(mail.sent()[0].attachment_existed);
        assert!(!lib.path().join(TEMP_DIR).join(row().meeting_id).exists());
    }

    #[test]
    fn the_startup_sweep_removes_the_whole_temp_directory() {
        let lib = tempfile::tempdir().unwrap();
        let left = lib.path().join(TEMP_DIR).join("m1");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::write(left.join("2026-01-02-sync.md"), b"x").unwrap();
        std::fs::write(lib.path().join("keep.txt"), b"x").unwrap();
        sweep_temp(lib.path()).unwrap();
        assert!(!lib.path().join(TEMP_DIR).exists());
        assert!(
            lib.path().join("keep.txt").exists(),
            "nothing else is touched"
        );
        sweep_temp(lib.path()).unwrap();
    }

    // ---------- the notice ----------

    struct FakeNotices {
        waiting: usize,
        attention: Option<i64>,
        last: Mutex<Option<i64>>,
    }

    impl NoticeStore for FakeNotices {
        fn waiting_count(&self) -> Result<usize, StoreError> {
            Ok(self.waiting)
        }
        fn last_attention_ms(&self) -> Result<Option<i64>, StoreError> {
            Ok(self.attention)
        }
        fn last_notice_ms(&self) -> Result<Option<i64>, StoreError> {
            Ok(*self.last.lock().unwrap())
        }
        fn record_notice(&self, at_ms: i64) -> Result<(), StoreError> {
            *self.last.lock().unwrap() = Some(at_ms);
            Ok(())
        }
    }

    const HOUR: i64 = NOTICE_INTERVAL_MS;

    #[test]
    fn the_notice_is_content_free() {
        let mail = FakeMail::default();
        let store = FakeNotices {
            waiting: 3,
            attention: Some(10),
            last: Mutex::new(None),
        };
        let outcome = send_notice(
            &mail,
            &store,
            &pass(),
            &allowed(),
            Some("owner@example.com"),
            100,
        );
        assert_eq!(outcome, NoticeOutcome::Sent { waiting: 3 });
        let sent = mail.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].subject,
            "FlyOnTheWall: 3 meetings waiting for release review"
        );
        assert_eq!(sent[0].body, "");
        assert_eq!(sent[0].attachment, None);
        assert_eq!(sent[0].to, vec!["owner@example.com"]);
        assert_eq!(*store.last.lock().unwrap(), Some(100));
        assert_eq!(
            notice_subject(1),
            "FlyOnTheWall: 1 meeting waiting for release review"
        );
    }

    #[test]
    fn the_notice_waits_an_hour_and_needs_something_new() {
        let mail = FakeMail::default();
        let addr = Some("owner@example.com");
        // Something new, but the last notice was 59 minutes ago.
        let recent = FakeNotices {
            waiting: 1,
            attention: Some(HOUR + 5),
            last: Mutex::new(Some(HOUR)),
        };
        assert_eq!(
            send_notice(&mail, &recent, &pass(), &allowed(), addr, 2 * HOUR - 1),
            NoticeOutcome::NotDue
        );
        // An hour on: due.
        assert_eq!(
            send_notice(&mail, &recent, &pass(), &allowed(), addr, 2 * HOUR),
            NoticeOutcome::Sent { waiting: 1 }
        );
        // Nothing entered review since that notice: not due, however long.
        assert_eq!(
            send_notice(&mail, &recent, &pass(), &allowed(), addr, 10 * HOUR),
            NoticeOutcome::NotDue
        );
        // Nothing waiting: not due.
        let empty = FakeNotices {
            waiting: 0,
            attention: Some(5),
            last: Mutex::new(None),
        };
        assert_eq!(
            send_notice(&mail, &empty, &pass(), &allowed(), addr, HOUR),
            NoticeOutcome::NotDue
        );
        assert_eq!(mail.sent().len(), 1);
    }

    #[test]
    fn a_failed_notice_records_nothing_and_is_retried() {
        let mail = FakeMail::answering(Err(MailError::Timeout));
        let store = FakeNotices {
            waiting: 2,
            attention: Some(10),
            last: Mutex::new(None),
        };
        let addr = Some("owner@example.com");
        assert_eq!(
            send_notice(&mail, &store, &pass(), &allowed(), addr, 100),
            NoticeOutcome::Failed {
                code: "timeout".to_owned()
            }
        );
        assert_eq!(*store.last.lock().unwrap(), None);
        assert_eq!(
            send_notice(&mail, &store, &pass(), &allowed(), addr, 101),
            NoticeOutcome::Sent { waiting: 2 }
        );
    }

    #[test]
    fn the_notice_goes_only_to_an_allowed_address() {
        let mail = FakeMail::default();
        let store = FakeNotices {
            waiting: 2,
            attention: Some(10),
            last: Mutex::new(None),
        };
        assert_eq!(
            send_notice(&mail, &store, &pass(), &allowed(), None, 100),
            NoticeOutcome::NoAddress
        );
        assert_eq!(
            send_notice(&mail, &store, &pass(), &allowed(), Some(""), 100),
            NoticeOutcome::NoAddress
        );
        assert_eq!(
            send_notice(
                &mail,
                &store,
                &pass(),
                &allowed(),
                Some("owner@example.org"),
                100
            ),
            NoticeOutcome::AddressNotAllowed
        );
        assert!(mail.sent().is_empty());
    }

    // ---------- logs ----------

    #[test]
    fn log_lines_carry_ids_states_and_codes_but_no_address_or_text() {
        let id = "01926f5a-0000-7000-8000-000000000001";
        let outcomes = [
            SendOutcome::NoRecipients,
            SendOutcome::LostRace,
            SendOutcome::Sent {
                sent_to: vec!["alice@example.com".to_owned()],
                message_id: Some("18c0ffee".to_owned()),
            },
            SendOutcome::GwsUnavailable,
            SendOutcome::SendRefused { http_status: 403 },
            SendOutcome::OutcomeUnknown {
                error: MailError::Other { code: "exit_auth" },
            },
        ];
        for outcome in outcomes {
            let line = outcome_line(id, &outcome);
            assert!(line.contains(id), "{line}");
            assert!(line.contains(outcome.state()), "{line}");
            if let Some(reason) = outcome.reason() {
                assert!(line.contains(reason), "{line}");
            }
            assert!(!line.contains('@'), "no address: {line}");
            assert!(!line.contains("18c0ffee"), "no message id: {line}");
        }
    }

    #[test]
    fn domain_matching_is_exact_and_case_insensitive() {
        let d = allowed();
        assert!(address_allowed("Alice@Example.COM", &d));
        assert!(!address_allowed("alice@sub.example.com", &d));
        assert!(!address_allowed("alice@example.com.evil.org", &d));
        assert!(!address_allowed("example.com", &d));
        assert!(!address_allowed("@example.com", &d));
        assert!(!address_allowed("a@b@example.com", &d));
    }

    // ---------- the chokepoint ----------

    /// The spelling of each guarded call, split so this file's own source does
    /// not match itself.
    const GUARDED: [&str; 2] = [
        concat!("MailRunner::", "send("),
        concat!("SendPermit::", "mint("),
    ];

    /// Every `(file, enclosing fn)` in `source` that spells a guarded call.
    fn guarded_calls(file: &str, source: &str) -> Vec<(String, String, &'static str)> {
        let mut current_fn = String::new();
        let mut calls = Vec::new();
        for line in source.lines() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if let Some(at) = code.find("fn ") {
                let before = &code[..at];
                if before.is_empty() || before.ends_with(' ') {
                    let name: String = code[at + 3..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        current_fn = name;
                    }
                }
            }
            for guarded in GUARDED {
                for _ in code.matches(guarded) {
                    calls.push((file.to_owned(), current_fn.clone(), guarded));
                }
            }
        }
        calls
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn send_one_and_send_notice_are_the_only_callers_of_mail_runner_send() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in ["src", "tests", "benches", "examples"] {
            if root.join(dir).is_dir() {
                rust_files(&root.join(dir), &mut files);
            }
        }
        let mut calls = Vec::new();
        for file in &files {
            let source = std::fs::read_to_string(file).unwrap();
            let rel = file.strip_prefix(root).unwrap().display().to_string();
            calls.extend(guarded_calls(&rel, &source));
        }
        calls.sort();
        let file = "src/release/mod.rs".to_owned();
        let mut expected = vec![
            (file.clone(), "send_notice".to_owned(), GUARDED[0]),
            (file.clone(), "send_notice".to_owned(), GUARDED[1]),
            (file.clone(), "send_one".to_owned(), GUARDED[0]),
            (file.clone(), "send_one".to_owned(), GUARDED[1]),
        ];
        expected.sort();
        assert_eq!(calls, expected);
    }

    #[test]
    fn the_caller_scan_catches_a_new_caller() {
        let src = format!(
            "fn send_one() {{\n    {}runner, p, c, m);\n}}\n\nfn sneaky() {{\n    let p = {});\n    x.{}p, c, m);\n}}\n",
            GUARDED[0], GUARDED[1], "send("
        );
        let calls = guarded_calls("src/x.rs", &src);
        assert!(
            calls
                .iter()
                .any(|(_, f, g)| f == "sneaky" && *g == GUARDED[1]),
            "{calls:?}"
        );
        assert!(calls.iter().any(|(_, f, _)| f == "send_one"), "{calls:?}");
    }
}
