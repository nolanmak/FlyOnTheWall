//! The first-run Recovery Key ceremony, and `fotwd recover`.
//!
//! `fotw-secrets::recovery` owns the cryptography; this module owns the part
//! that is actually hard, which is making a human end up with a string on paper.
//!
//! # "Unskippable" is not the same as "cannot be dismissed"
//!
//! docs/REQUIREMENTS.md 10 says the Recovery Key dialog cannot be skipped. A
//! modal with one button satisfies that sentence and satisfies nothing else:
//! the user clicks through, the key scrolls off the screen, and the outcome is
//! identical to never having shown it. The point of the dialog is not that it
//! appeared, it is that the key left the machine — so what is enforced here is
//! **evidence**, not acknowledgement:
//!
//! 1. The key is displayed.
//! 2. Two *different*, randomly chosen groups have to be typed back. Reading
//!    them off a screen that is still showing them is possible, and that is
//!    fine — it means they have looked at it character by character, which is
//!    what transcription failures come from. What it rules out is the person
//!    who pressed return without reading anything.
//! 3. A literal phrase has to be typed. Not "press y" — a phrase, so that the
//!    muscle-memory return keypress cannot get through it.
//!
//! And crucially: **if any of that fails, nothing is created.** No database, no
//! sealed file, no keychain entry. A first run that ends in a refusal leaves a
//! machine that has never had a library, which is recoverable; a first run that
//! ends in a library whose key nobody has is not.
//!
//! # The headless case
//!
//! A run with no terminal — launchd, CI, `ssh host fotwd serve` — has nobody to
//! show a key to. Creating a library there would mint one that is by
//! construction unrecoverable, so it refuses. The escape hatch for automation
//! is [`UNATTENDED_ENV`], and it is deliberately awkward to set by accident.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use fotw_secrets::recovery::{
    GROUP_COUNT, KdfParams, MasterKeyBytes, RecoveryError, RecoveryKey, WrappedMasterKey,
    blob_path_for, random_bytes,
};
use fotw_secrets::{KeyStore, SecretString, SecretsError};
use fotw_store::Db;

use crate::secrets;

/// The environment variable that lets an unattended run create a library.
///
/// Set it to [`UNATTENDED_VALUE`]. It prints the Recovery Key to stdout, which
/// on a CI runner means into the build log — which is why the value is a
/// sentence rather than `1`. Nobody sets this by accident, and nobody sets it
/// without having read what it does.
pub const UNATTENDED_ENV: &str = "FOTW_RECOVERY_UNATTENDED";

/// The one accepted value of [`UNATTENDED_ENV`].
pub const UNATTENDED_VALUE: &str = "print-the-key-to-stdout";

/// The phrase the user has to type to finish the ceremony.
pub const ACKNOWLEDGEMENT: &str = "i have written it down";

/// Attempts allowed per confirmation group before the ceremony gives up.
const ATTEMPTS_PER_GROUP: usize = 3;

/// Groups the user must type back.
const GROUPS_CHALLENGED: usize = 2;

/// Cheap Argon2 parameters for tests.
///
/// Public so integration tests in this crate and in `fotwd/tests` can share
/// one definition. Shipping code passes [`KdfParams::default`]; the cost
/// parameters are an input to Argon2, not to any logic under test, and a suite
/// that paid 200 ms per unwrap would be a suite nobody runs.
pub const KDF_FOR_TESTS: KdfParams = KdfParams {
    m_cost_kib: 64,
    t_cost: 1,
    p_cost: 1,
};

/// Everything the ceremony needs from the outside world.
///
/// A trait, not a pile of `println!`s, for one reason: the ceremony is the part
/// of this feature most likely to be broken by a well-meaning edit, and it is
/// unreachable from a test if it talks to a terminal directly. The real
/// implementation is [`TtyCeremony`]; the tests script it.
pub trait Ceremony {
    /// Whether there is a human on the other end.
    ///
    /// Checked **before** a Recovery Key is generated, so a headless run never
    /// creates a secret it has nowhere to put.
    fn is_interactive(&self) -> bool;

    /// Whether the confirmation challenge is meaningful.
    ///
    /// False only in the explicitly-opted-into unattended mode, where there is
    /// nobody to challenge. Everything else says true, and the default is true
    /// so that a new implementation cannot weaken the ceremony by omission.
    fn confirms(&self) -> bool {
        true
    }

    /// Narrative text. Never secret.
    fn tell(&mut self, text: &str);

    /// Display the Recovery Key.
    ///
    /// **The only place it ever becomes visible.** `rg 'fn reveal'` is the
    /// audit.
    fn reveal(&mut self, key: &SecretString);

    /// Ask for group `index` (0-based) back.
    ///
    /// # Errors
    ///
    /// Any I/O failure. `enroll` treats one as "there is nobody here" rather
    /// than retrying: if we cannot reach the user, we cannot confirm anything.
    fn ask_group(&mut self, index: usize, attempt: usize) -> std::io::Result<String>;

    /// Ask the user to type `phrase`.
    ///
    /// # Errors
    ///
    /// As [`Ceremony::ask_group`].
    fn ask_acknowledgement(&mut self, phrase: &str) -> std::io::Result<String>;
}

/// Why a first run did not produce a library.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CeremonyError {
    /// The user was there and did not complete the ceremony.
    #[error(
        "the Recovery Key was not confirmed, so no library was created: {0}\n  \
         Nothing has been written. Run the command again when you have somewhere \
         to write the key down."
    )]
    Declined(String),

    /// There was nobody to show the key to.
    #[error(
        "this run has no terminal, so there is nobody to show a Recovery Key to, \
         and a library created here could never be recovered: {0}\n  \
         Create the library once from a terminal, or set {UNATTENDED_ENV}={UNATTENDED_VALUE} \
         to accept that the key will be printed to stdout (and therefore into \
         whatever captures it)."
    )]
    NoHuman(String),

    /// The cryptography or the sealed file failed.
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
}

impl CeremonyError {
    /// True when a human refused or could not confirm.
    #[must_use]
    pub fn is_declined(&self) -> bool {
        matches!(self, Self::Declined(_))
    }

    /// True when there was no human at all.
    #[must_use]
    pub fn is_no_human(&self) -> bool {
        matches!(self, Self::NoHuman(_))
    }
}

/// Mint a Recovery Key for `master`, prove the user has it, and seal the key.
///
/// Order is the whole design: the sealed file is written **last**, after every
/// confirmation has passed, so an aborted ceremony leaves the filesystem
/// exactly as it found it.
///
/// # Errors
///
/// [`CeremonyError::NoHuman`] before anything is generated when there is no
/// terminal; [`CeremonyError::Declined`] when the confirmations fail;
/// [`CeremonyError::Recovery`] if sealing or writing fails.
pub fn enroll(
    ceremony: &mut dyn Ceremony,
    master: &MasterKeyBytes,
    blob_path: &Path,
    kdf: KdfParams,
    first_run: bool,
) -> Result<(), CeremonyError> {
    if !ceremony.is_interactive() {
        return Err(CeremonyError::NoHuman("no terminal is attached".to_owned()));
    }

    let recovery = RecoveryKey::generate()?;
    let shown = recovery.display_string()?;

    // Seal before displaying: if the cryptography is going to fail, it should
    // fail before the user has copied anything down.
    let blob = WrappedMasterKey::wrap(master, &recovery, kdf)?;

    preamble(ceremony, first_run);
    ceremony.reveal(&shown);
    postamble(ceremony, blob_path);

    if ceremony.confirms() {
        challenge(ceremony, &recovery)?;
        acknowledge(ceremony)?;
    } else {
        ceremony.tell(
            "  ! Confirmation was skipped: this run is unattended, so there is nobody\n    \
             to challenge. The key above is now in this process's output.",
        );
    }

    blob.write_to(blob_path)?;
    ceremony.tell("");
    ceremony.tell("  ✓ Recovery Key confirmed. Your library is ready.");
    Ok(())
}

fn preamble(ceremony: &mut dyn Ceremony, first_run: bool) {
    ceremony.tell("");
    ceremony.tell("  ─── Your Recovery Key ─────────────────────────────────────────");
    ceremony.tell("");
    if first_run {
        ceremony.tell("  FlyOnTheWall has just created your meeting library. Everything in");
        ceremony.tell("  it — transcripts, notes, summaries, audio — is encrypted with a key");
        ceremony.tell("  stored in your keychain.");
    } else {
        ceremony.tell("  Your library already exists and is already encrypted. It has never");
        ceremony.tell("  had a Recovery Key, so it is one lost keychain entry away from being");
        ceremony.tell("  unreadable forever. This fixes that, and does not re-encrypt or move");
        ceremony.tell("  a single byte of your data.");
    }
    ceremony.tell("");
    ceremony.tell("  If that keychain entry is ever lost — a wiped machine, a restore onto");
    ceremony.tell("  new hardware, a keychain that no longer recognises this app — the key");
    ceremony.tell("  below is the ONLY other way in. There is no reset and no backdoor. We");
    ceremony.tell("  cannot recover your library for you, and that is the point.");
    ceremony.tell("");
    ceremony.tell("  Write it down on paper. Not a screenshot inside the library it opens,");
    ceremony.tell("  and not only in a password manager that lives on this machine.");
    ceremony.tell("");
}

fn postamble(ceremony: &mut dyn Ceremony, blob_path: &Path) {
    ceremony.tell("");
    ceremony.tell(&format!(
        "  A sealed copy of your library key is stored in\n    {}",
        blob_path.display()
    ));
    ceremony.tell("  That file cannot open anything without the Recovery Key above — it is");
    ceremony.tell("  the lock, not the key. Back it up together with db.sqlite3.");
    ceremony.tell("");
}

/// Ask for [`GROUPS_CHALLENGED`] distinct groups, chosen at random.
///
/// Random rather than fixed, so that "the first and last block" cannot become
/// folklore that people copy down instead of the whole key.
fn challenge(ceremony: &mut dyn Ceremony, recovery: &RecoveryKey) -> Result<(), CeremonyError> {
    ceremony.tell("  Type two of the groups back, so we both know you have it.");

    for index in pick_groups()? {
        let mut passed = false;
        for attempt in 0..ATTEMPTS_PER_GROUP {
            let typed = ceremony
                .ask_group(index, attempt)
                .map_err(|e| CeremonyError::NoHuman(e.to_string()))?;
            if recovery.group_matches(index, &typed)? {
                passed = true;
                break;
            }
            ceremony.tell(&format!(
                "  ✗ that is not group {}. {} attempt(s) left.",
                index + 1,
                ATTEMPTS_PER_GROUP - attempt - 1
            ));
        }
        if !passed {
            return Err(CeremonyError::Declined(format!(
                "group {} was not typed back correctly",
                index + 1
            )));
        }
    }
    Ok(())
}

fn acknowledge(ceremony: &mut dyn Ceremony) -> Result<(), CeremonyError> {
    let typed = ceremony
        .ask_acknowledgement(ACKNOWLEDGEMENT)
        .map_err(|e| CeremonyError::NoHuman(e.to_string()))?;
    if typed.trim().eq_ignore_ascii_case(ACKNOWLEDGEMENT) {
        return Ok(());
    }
    Err(CeremonyError::Declined(format!(
        "the confirmation phrase was not typed (expected `{ACKNOWLEDGEMENT}`)"
    )))
}

/// Two distinct group indices from the CSPRNG.
fn pick_groups() -> Result<Vec<usize>, RecoveryError> {
    let entropy: [u8; 8] = random_bytes()?;
    let mut chosen: Vec<usize> = Vec::with_capacity(GROUPS_CHALLENGED);
    for byte in entropy {
        let candidate = usize::from(byte) % GROUP_COUNT;
        if !chosen.contains(&candidate) {
            chosen.push(candidate);
        }
        if chosen.len() == GROUPS_CHALLENGED {
            return Ok(chosen);
        }
    }
    // Eight random bytes failing to yield two distinct values out of eight is
    // a 1-in-2^21 event; fill deterministically rather than loop forever.
    for candidate in 0..GROUP_COUNT {
        if !chosen.contains(&candidate) {
            chosen.push(candidate);
        }
        if chosen.len() == GROUPS_CHALLENGED {
            break;
        }
    }
    Ok(chosen)
}

// ------------------------------------------------------------- fotwd recover

/// What `fotwd recover` produced.
#[derive(Debug)]
pub struct Recovered {
    /// The opened library. Proof, not a promise: the master key that came out
    /// of the sealed file was actually handed to SQLCipher and it read page 1.
    pub db: Db,
    /// Whether the master key was put back in the keychain, so the next
    /// ordinary run needs no Recovery Key.
    pub key_restored_to_keychain: bool,
}

/// Why a recovery attempt failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecoverError {
    /// The Recovery Key, the sealed file, or the cryptography.
    #[error(transparent)]
    Recovery(#[from] RecoveryError),

    /// The sealed file opened, but the key it yielded does not open the
    /// database beside it.
    ///
    /// Its own variant because it is the one case where the *pairing* is
    /// wrong: a `db.sqlite3` from one install next to a `db.sqlite3.recovery`
    /// from another, which happens when someone reassembles a backup by hand.
    #[error(
        "the Recovery Key opened the sealed file, but the key inside it does not \
         open {db}.\n  \
         The recovery file and the database do not belong together — they are \
         probably from different backups. Find the db.sqlite3 that was saved \
         alongside this db.sqlite3.recovery.\n  \
         (underlying error: {detail})"
    )]
    Mismatched {
        /// The database that refused the recovered key.
        db: PathBuf,
        /// What SQLCipher said.
        detail: String,
    },

    /// The library was recovered but the key could not be put back.
    #[error("the library opened, but the master key could not be stored in the keychain: {0}")]
    Keychain(#[from] SecretsError),
}

impl RecoverError {
    /// True when the typed key was not a well-formed Recovery Key.
    #[must_use]
    pub fn is_malformed(&self) -> bool {
        matches!(self, Self::Recovery(e) if e.is_malformed())
    }

    /// True when the key was well-formed but is not this library's.
    #[must_use]
    pub fn is_wrong_key(&self) -> bool {
        matches!(self, Self::Recovery(e) if e.is_wrong_key())
    }

    /// True when there is no sealed file to recover from.
    #[must_use]
    pub fn is_missing_blob(&self) -> bool {
        matches!(self, Self::Recovery(e) if e.is_missing_blob())
    }

    /// True when the sealed file is damaged.
    #[must_use]
    pub fn is_corrupt_blob(&self) -> bool {
        matches!(self, Self::Recovery(e) if e.is_corrupt_blob())
    }
}

/// Open the library at `data_root` with a Recovery Key, and put the master key
/// back in the keychain.
///
/// The order matters and is the opposite of the obvious one: **open the
/// database first, store the key second.** Writing to the keychain before
/// proving the key works would leave a machine claiming to hold a master key
/// that opens nothing — and the next ordinary run would then sail past the
/// "no key here" guard and hand SQLCipher the wrong key.
///
/// # Errors
///
/// See [`RecoverError`]. Every variant is distinguishable, and none of them is
/// "the database is corrupt".
pub fn recover(
    data_root: &Path,
    store: &dyn KeyStore,
    typed: &str,
) -> Result<Recovered, RecoverError> {
    let db_path = data_root.join("db.sqlite3");
    let blob_path = blob_path_for(&db_path);

    // Parse first: a typo costs nothing to detect and is by far the likeliest
    // failure, so it must not be reported as anything more alarming.
    let recovery = RecoveryKey::parse(typed)?;
    let blob = WrappedMasterKey::read_from(&blob_path)?;
    let master = blob.unwrap_master(&recovery, &blob_path)?;

    let key = secrets::db_key_of(&master)?;
    let db = Db::open(&db_path, &key).map_err(|e| RecoverError::Mismatched {
        db: db_path.clone(),
        detail: e.to_string(),
    })?;

    secrets::store_master_key(store, &master)?;
    Ok(Recovered {
        db,
        key_restored_to_keychain: true,
    })
}

/// Verify a written-down Recovery Key.
///
/// What a careful user does a month later: check that the card in the drawer
/// still works. It touches neither the keychain nor the sealed file, which is
/// why it is a separate function rather than a flag on [`recover`] — a "check"
/// that silently re-wrote the keychain would be a check nobody could trust.
///
/// It is not *completely* side-effect free, and the difference matters enough
/// to write down: opening the library applies any pending schema migration,
/// exactly as an ordinary run would. That is a property of `Db::open`, not of
/// this function, and the alternative — checking only that the sealed file
/// unwraps — would answer a weaker question than the one the user is asking.
///
/// # Errors
///
/// As [`recover`], minus the keychain.
pub fn check(data_root: &Path, typed: &str) -> Result<(), RecoverError> {
    let db_path = data_root.join("db.sqlite3");
    let blob_path = blob_path_for(&db_path);

    let recovery = RecoveryKey::parse(typed)?;
    let blob = WrappedMasterKey::read_from(&blob_path)?;
    let master = blob.unwrap_master(&recovery, &blob_path)?;

    // If there is a library here, prove the key opens it too. Unwrapping the
    // blob only proves the blob is intact; the pairing is the thing a user
    // actually wants checked.
    if db_path.exists() {
        let key = secrets::db_key_of(&master)?;
        Db::open(&db_path, &key).map_err(|e| RecoverError::Mismatched {
            db: db_path,
            detail: e.to_string(),
        })?;
    }
    Ok(())
}

// ----------------------------------------------------------- the real dialog

/// The [`Ceremony`] a user sees.
///
/// Reads from stdin and writes to stderr — **stderr for the narrative, stdout
/// for the key** — so that `fotwd ... > /dev/null` cannot silently discard the
/// one line that matters, and so a user piping the output somewhere gets the
/// key rather than the prose.
#[derive(Debug, Default)]
pub struct TtyCeremony {
    /// Read once at construction rather than per call, so the answer cannot
    /// change under a ceremony that is halfway through.
    unattended: bool,
}

impl TtyCeremony {
    /// A ceremony bound to the process's own terminal.
    #[must_use]
    pub fn new() -> Self {
        Self {
            unattended: std::env::var(UNATTENDED_ENV).is_ok_and(|v| v == UNATTENDED_VALUE),
        }
    }
}

impl Ceremony for TtyCeremony {
    fn is_interactive(&self) -> bool {
        self.unattended || std::io::stdin().is_terminal()
    }

    fn confirms(&self) -> bool {
        !self.unattended
    }

    fn tell(&mut self, text: &str) {
        eprintln!("{text}");
    }

    fn reveal(&mut self, key: &SecretString) {
        // The single `expose()` call site for a Recovery Key in the whole
        // shipping tree. It goes to stdout, unindented, on a line of its own,
        // so it can be selected cleanly with a mouse.
        println!("      {}", key.expose());
        let _ = std::io::stdout().flush();
    }

    fn ask_group(&mut self, index: usize, attempt: usize) -> std::io::Result<String> {
        let suffix = if attempt == 0 { "" } else { " (again)" };
        eprint!(
            "  group {} of {GROUP_COUNT} — the {} block of four{suffix}: ",
            index + 1,
            ordinal(index + 1)
        );
        std::io::stderr().flush()?;
        read_line()
    }

    fn ask_acknowledgement(&mut self, phrase: &str) -> std::io::Result<String> {
        eprintln!();
        eprintln!("  Type `{phrase}` to finish.");
        eprint!("  > ");
        std::io::stderr().flush()?;
        read_line()
    }
}

/// One line from stdin, or an error when stdin has ended.
///
/// EOF is an error rather than an empty string: a closed stdin means there is
/// nobody there, and treating it as a blank answer would burn the retries and
/// report "declined" for a run that never had a human in it.
fn read_line() -> std::io::Result<String> {
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line)?;
    if read == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "stdin closed",
        ));
    }
    Ok(line)
}

fn ordinal(n: usize) -> &'static str {
    match n {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        5 => "fifth",
        6 => "sixth",
        7 => "seventh",
        _ => "eighth",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_picks_two_distinct_groups_in_range() {
        for _ in 0..500 {
            let groups = pick_groups().unwrap();
            assert_eq!(groups.len(), GROUPS_CHALLENGED);
            assert_ne!(groups[0], groups[1], "the same group twice: {groups:?}");
            assert!(groups.iter().all(|g| *g < GROUP_COUNT), "{groups:?}");
        }
    }

    /// Over many runs every group must be reachable, or "randomly chosen"
    /// would be a comment rather than a behaviour.
    #[test]
    fn every_group_is_reachable() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2_000 {
            seen.extend(pick_groups().unwrap());
        }
        assert_eq!(seen.len(), GROUP_COUNT, "unreachable groups: {seen:?}");
    }

    /// The unattended value has to be an exact match. `FOTW_RECOVERY_UNATTENDED=0`
    /// enabling it would be the worst possible bug in this file.
    #[test]
    fn the_unattended_switch_needs_the_exact_phrase() {
        assert_ne!(UNATTENDED_VALUE, "1");
        assert_ne!(UNATTENDED_VALUE, "true");
        assert!(UNATTENDED_VALUE.len() > 10);
    }

    #[test]
    fn a_ceremony_that_does_not_override_confirms_still_confirms() {
        struct Minimal;
        impl Ceremony for Minimal {
            fn is_interactive(&self) -> bool {
                true
            }
            fn tell(&mut self, _: &str) {}
            fn reveal(&mut self, _: &SecretString) {}
            fn ask_group(&mut self, _: usize, _: usize) -> std::io::Result<String> {
                Ok(String::new())
            }
            fn ask_acknowledgement(&mut self, _: &str) -> std::io::Result<String> {
                Ok(String::new())
            }
        }
        assert!(
            Minimal.confirms(),
            "a new Ceremony can weaken the dialog by omission"
        );
    }
}
