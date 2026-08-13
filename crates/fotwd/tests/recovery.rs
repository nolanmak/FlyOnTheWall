//! Issue #38, end to end: **a library encrypted normally must open with the
//! Recovery Key alone once the keychain entry is gone.**
//!
//! That round trip is the entire feature. Everything else in this tree —
//! bech32m, Argon2id, the sealed blob, the first-run ceremony — is a key-shaped
//! object that has never been shown to work until this file passes.
//!
//! # How this avoids being another vacuous acceptance test
//!
//! The failure mode this repository keeps producing is a test that would pass
//! against an implementation doing nothing. Three things make that impossible
//! here:
//!
//! 1. **The library is created through the ordinary path**, not a fixture. The
//!    same `open_library_with` every `fotwd` command calls mints the key, runs
//!    the ceremony and writes the blob. There is no test-only door into the
//!    encrypted state.
//! 2. **The keychain entry is really destroyed** between writing and reading,
//!    and there is an assertion that the normal path fails afterwards. If the
//!    master key were reachable some other way, that assertion fails and the
//!    recovery below would prove nothing.
//! 3. **The data read back is checked**, not just the fact that a handle
//!    opened. A wrong SQLCipher key does not produce an empty database, it
//!    produces an error — but a bug that quietly re-created the library would
//!    produce an empty one, so the meeting has to still be there.

use std::path::Path;

use fotw_secrets::recovery::{MasterKeyBytes, RecoveryKey, WrappedMasterKey, blob_path_for};
use fotw_secrets::{InMemoryKeyStore, KeyStore, SecretKey, SecretString};
use fotw_store::NewMeeting;
use fotwd::recovery::{Ceremony, CeremonyError, KDF_FOR_TESTS, recover};

// ------------------------------------------------------------ a test double

/// A [`Ceremony`] driven by a script, so the first-run dialog can be exercised
/// without a terminal.
///
/// It captures what was revealed, which is how the round trip below gets the
/// Recovery Key — exactly as a user would: off the screen, once.
#[derive(Default)]
struct Scripted {
    /// The Recovery Key as the user would have seen it.
    revealed: Option<String>,
    /// Everything the ceremony said, for assertions about the copy.
    transcript: Vec<String>,
    /// When set, answer every group challenge with this instead of the truth.
    wrong_group_answer: Option<String>,
    /// What to type at the acknowledgement prompt.
    acknowledgement: Option<String>,
    /// How many group challenges were put to the user.
    challenges: usize,
    /// How many times the key was displayed.
    reveals: usize,
}

impl Scripted {
    fn new() -> Self {
        Self::default()
    }

    fn refusing_to_confirm() -> Self {
        Self {
            wrong_group_answer: Some("zzzz".to_owned()),
            ..Self::default()
        }
    }

    fn refusing_to_acknowledge() -> Self {
        Self {
            acknowledgement: Some("no".to_owned()),
            ..Self::default()
        }
    }

    /// The Recovery Key the user was shown.
    fn key(&self) -> &str {
        self.revealed
            .as_deref()
            .expect("the ceremony never displayed a Recovery Key")
    }

    fn said(&self, needle: &str) -> bool {
        self.transcript
            .iter()
            .any(|l| l.to_lowercase().contains(needle))
    }
}

impl Ceremony for Scripted {
    fn is_interactive(&self) -> bool {
        true
    }

    fn tell(&mut self, text: &str) {
        self.transcript.push(text.to_owned());
    }

    fn reveal(&mut self, key: &SecretString) {
        self.reveals += 1;
        self.revealed = Some(key.expose().to_owned());
    }

    fn ask_group(&mut self, index: usize, _attempt: usize) -> std::io::Result<String> {
        self.challenges += 1;
        if let Some(wrong) = &self.wrong_group_answer {
            return Ok(wrong.clone());
        }
        // Read the answer off the "card", like a user would.
        let shown = self.key().strip_prefix("fotw1-").unwrap();
        Ok(shown.split('-').nth(index).unwrap().to_owned())
    }

    fn ask_acknowledgement(&mut self, phrase: &str) -> std::io::Result<String> {
        Ok(self
            .acknowledgement
            .clone()
            .unwrap_or_else(|| phrase.to_owned()))
    }
}

// ------------------------------------------------------------------ helpers

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    store: InMemoryKeyStore,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("com.flyonthewall.fotw");
        std::fs::create_dir_all(&root).unwrap();
        Self {
            _dir: dir,
            root,
            store: InMemoryKeyStore::new(),
        }
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.root.join("db.sqlite3")
    }

    fn blob_path(&self) -> std::path::PathBuf {
        blob_path_for(&self.db_path())
    }

    /// Create the library the way the daemon does, returning the ceremony so
    /// the test can read the Recovery Key off it.
    fn create(&self) -> Scripted {
        let mut ceremony = Scripted::new();
        let mut db =
            fotwd::open_library_with(&self.root, &self.store, &mut ceremony, KDF_FOR_TESTS)
                .expect("first run failed");
        db.meetings()
            .create(
                NewMeeting::new("device-under-test", "Europe/Berlin")
                    .title("Quarterly review with Priya"),
            )
            .expect("could not write the fixture meeting");
        ceremony
    }

    /// Everything the app wrote, so a test can prove a key is not among it.
    fn all_bytes(&self) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    walk(&path, out);
                } else if entry.file_type().unwrap().is_file() {
                    let bytes = std::fs::read(&path).unwrap();
                    out.push((path, bytes));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }
}

fn meeting_titles(db: &mut fotw_store::Db) -> Vec<String> {
    db.meetings()
        .list(50, 0)
        .unwrap()
        .into_iter()
        .map(|m| m.title)
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

// ------------------------------------------------------------ THE round trip

/// Create a library the ordinary way, destroy the keychain entry, and open it
/// again with nothing but the written-down Recovery Key.
#[test]
fn a_normally_encrypted_library_opens_with_only_the_recovery_key() {
    let f = Fixture::new();
    let ceremony = f.create();
    let recovery_key = ceremony.key().to_owned();

    assert!(f.db_path().exists(), "no database was created");
    assert!(f.blob_path().exists(), "no recovery file was written");
    assert!(
        f.store.contains(SecretKey::DbMasterKey).unwrap(),
        "the master key was never stored in the keychain"
    );

    // The disaster: the keychain is gone. A wiped machine, a restore onto new
    // hardware, or an ACL bound to a code signature that no longer matches
    // (issue #53).
    f.store.delete(SecretKey::DbMasterKey).unwrap();
    assert!(!f.store.contains(SecretKey::DbMasterKey).unwrap());

    // Guard 2: the ordinary path must now fail. If it succeeded, the master
    // key would be reachable without the Recovery Key and everything below
    // would prove nothing.
    let mut ceremony = Scripted::new();
    let err = fotwd::open_library_with(&f.root, &f.store, &mut ceremony, KDF_FOR_TESTS)
        .expect_err("the library opened with no key in the keychain");
    assert!(
        err.contains("fotwd recover"),
        "the refusal does not point at the recovery command: {err}"
    );

    // And it must NOT have minted a replacement key, which is the failure that
    // turns a lost keychain into "file is not a database".
    assert!(
        !f.store.contains(SecretKey::DbMasterKey).unwrap(),
        "a new master key was generated over an existing library"
    );

    // Now recover, with the string the user wrote down and nothing else.
    let outcome = recover(&f.root, &f.store, &recovery_key).expect("recovery failed");
    assert!(outcome.key_restored_to_keychain);

    // Guard 3: the data is actually there. A bug that re-created the library
    // would open cleanly and be empty.
    let mut db = outcome.db;
    assert_eq!(
        meeting_titles(&mut db),
        vec!["Quarterly review with Priya".to_owned()],
        "the recovered library is not the one we wrote"
    );
    drop(db);

    // ...and the next ordinary run is normal again, with no ceremony.
    let mut ceremony = Scripted::new();
    let mut db = fotwd::open_library_with(&f.root, &f.store, &mut ceremony, KDF_FOR_TESTS)
        .expect("the library did not open normally after recovery");
    assert_eq!(ceremony.reveals, 0, "the ceremony ran a second time");
    assert_eq!(
        meeting_titles(&mut db),
        vec!["Quarterly review with Priya".to_owned()]
    );
}

/// The negative that gives the round trip its meaning. Also the error-class
/// test issue #38 is explicit about: **not** "database is corrupt".
#[test]
fn a_wrong_recovery_key_is_a_wrong_key_and_says_so() {
    let f = Fixture::new();
    f.create();
    f.store.delete(SecretKey::DbMasterKey).unwrap();

    // A different, perfectly well-formed Recovery Key.
    let other = RecoveryKey::generate()
        .unwrap()
        .display_string()
        .unwrap()
        .expose()
        .to_owned();

    let err = recover(&f.root, &f.store, &other).expect_err("a foreign Recovery Key opened it");
    let text = err.to_string();

    assert!(err.is_wrong_key(), "wrong error class: {err:?}");
    assert!(
        text.contains("NOT corrupt"),
        "the message lets the user think their data is damaged: {text}"
    );
    assert!(
        !text.contains("file is not a database"),
        "the message repeats SQLCipher's misleading wording: {text}"
    );
    assert!(
        !f.store.contains(SecretKey::DbMasterKey).unwrap(),
        "a failed recovery wrote a key to the keychain"
    );
}

/// A typo is a different failure from a wrong key, and gets a different
/// message: nothing has been tried against the library yet.
#[test]
fn a_mistyped_recovery_key_is_reported_as_a_typo() {
    let f = Fixture::new();
    let ceremony = f.create();
    f.store.delete(SecretKey::DbMasterKey).unwrap();

    // One character wrong.
    let mut typed: Vec<char> = ceremony.key().chars().collect();
    let last = typed.len() - 1;
    typed[last] = if typed[last] == 'q' { 'p' } else { 'q' };
    let typed: String = typed.into_iter().collect();

    let err = recover(&f.root, &f.store, &typed).expect_err("a mistyped key was accepted");
    assert!(err.is_malformed(), "wrong error class: {err:?}");
    assert!(
        err.to_string().contains("Nothing was tried"),
        "unhelpful: {err}"
    );
}

/// Recovery with no sealed file is its own error — the Recovery Key alone
/// cannot open anything, and saying "wrong key" there would be a lie.
#[test]
fn recovery_without_the_sealed_file_says_the_file_is_missing() {
    let f = Fixture::new();
    let ceremony = f.create();
    let key = ceremony.key().to_owned();
    f.store.delete(SecretKey::DbMasterKey).unwrap();
    std::fs::remove_file(f.blob_path()).unwrap();

    let err = recover(&f.root, &f.store, &key).expect_err("recovered with no sealed file");
    assert!(err.is_missing_blob(), "{err:?}");
    assert!(err.to_string().contains("db.sqlite3.recovery"));
}

// -------------------------------------------------------- the dialog is real

/// "Unskippable" has to mean more than a prompt the user hits return on. A
/// ceremony that cannot produce the key back gets nothing: no database, no
/// sealed file, no keychain entry.
#[test]
fn a_user_who_cannot_confirm_the_key_gets_no_library_at_all() {
    let f = Fixture::new();
    let mut ceremony = Scripted::refusing_to_confirm();

    let err = fotwd::open_library_with(&f.root, &f.store, &mut ceremony, KDF_FOR_TESTS)
        .expect_err("a library was created without a confirmed Recovery Key");
    assert!(err.to_lowercase().contains("recovery key"), "{err}");

    assert!(ceremony.challenges >= 2, "the user was barely challenged");
    assert!(
        !f.db_path().exists(),
        "a database was left behind by an aborted first run"
    );
    assert!(!f.blob_path().exists(), "a sealed file was left behind");
    assert!(
        !f.store.contains(SecretKey::DbMasterKey).unwrap(),
        "a master key was stored for a library that does not exist"
    );
}

/// The same for the acknowledgement: the user has to type the phrase, not
/// press return.
#[test]
fn a_user_who_will_not_acknowledge_gets_no_library_either() {
    let f = Fixture::new();
    let mut ceremony = Scripted::refusing_to_acknowledge();

    assert!(
        fotwd::open_library_with(&f.root, &f.store, &mut ceremony, KDF_FOR_TESTS).is_err(),
        "an unacknowledged first run created a library"
    );
    assert!(!f.db_path().exists());
    assert!(!f.blob_path().exists());
}

/// Confirmation means re-entering part of the key, and the challenge covers
/// more than one group — one group is 20 bits and could be guessed by someone
/// who never wrote anything down.
#[test]
fn confirmation_requires_typing_back_at_least_two_distinct_groups() {
    let f = Fixture::new();
    let mut ceremony = Scripted::new();
    let mut seen = Vec::new();

    /// Records which groups were asked for.
    struct Recording<'a> {
        inner: &'a mut Scripted,
        asked: &'a mut Vec<usize>,
    }
    impl Ceremony for Recording<'_> {
        fn is_interactive(&self) -> bool {
            self.inner.is_interactive()
        }
        fn tell(&mut self, text: &str) {
            self.inner.tell(text);
        }
        fn reveal(&mut self, key: &SecretString) {
            self.inner.reveal(key);
        }
        fn ask_group(&mut self, index: usize, attempt: usize) -> std::io::Result<String> {
            self.asked.push(index);
            self.inner.ask_group(index, attempt)
        }
        fn ask_acknowledgement(&mut self, phrase: &str) -> std::io::Result<String> {
            self.inner.ask_acknowledgement(phrase)
        }
    }

    {
        let mut recording = Recording {
            inner: &mut ceremony,
            asked: &mut seen,
        };
        fotwd::open_library_with(&f.root, &f.store, &mut recording, KDF_FOR_TESTS).unwrap();
    }

    assert!(
        seen.len() >= 2,
        "only {} group(s) were asked for",
        seen.len()
    );
    let distinct: std::collections::BTreeSet<usize> = seen.iter().copied().collect();
    assert!(
        distinct.len() >= 2,
        "the same group was asked for twice: {seen:?}"
    );
    // The copy has to name the consequence, not just ask for a keypress.
    assert!(
        ceremony.said("write it down"),
        "the copy never tells the user to write it down"
    );
    assert!(
        ceremony.said("only other way") || ceremony.said("no reset"),
        "the copy never says what is lost without it"
    );
}

/// A headless run — CI, launchd, ssh — has nobody to show the key to. Minting
/// a library there would create one that is unrecoverable by construction, so
/// it refuses instead.
#[test]
fn a_run_with_no_human_refuses_to_create_a_library() {
    /// The real `TtyCeremony`'s behaviour when there is no terminal, without
    /// needing to actually detach one.
    struct NoHuman;
    impl Ceremony for NoHuman {
        fn is_interactive(&self) -> bool {
            false
        }
        fn tell(&mut self, _: &str) {}
        fn reveal(&mut self, _: &SecretString) {
            panic!("a headless ceremony displayed the Recovery Key");
        }
        fn ask_group(&mut self, _: usize, _: usize) -> std::io::Result<String> {
            panic!("a headless ceremony challenged a user who is not there");
        }
        fn ask_acknowledgement(&mut self, _: &str) -> std::io::Result<String> {
            panic!("a headless ceremony asked for an acknowledgement");
        }
    }

    let f = Fixture::new();
    let err = fotwd::open_library_with(&f.root, &f.store, &mut NoHuman, KDF_FOR_TESTS)
        .expect_err("a library was created with nobody to receive the Recovery Key");
    assert!(err.to_lowercase().contains("recovery key"), "{err}");
    assert!(!f.db_path().exists());
    assert!(!f.blob_path().exists());
    assert!(!f.store.contains(SecretKey::DbMasterKey).unwrap());
}

// ------------------------------------------- wrap, don't replace; and rotate

/// A library that predates issue #38 has a master key and no sealed file. It
/// must gain one **without being re-encrypted** — that is the whole reason the
/// Recovery Key wraps the master key instead of being it.
#[test]
fn an_existing_library_is_enrolled_without_re_encrypting_it() {
    let f = Fixture::new();
    f.create();

    // Roll back to the pre-#38 world: key in the keychain, no sealed file.
    let before = std::fs::read(f.db_path()).unwrap();
    std::fs::remove_file(f.blob_path()).unwrap();
    let stored_key_before = f.store.get(SecretKey::DbMasterKey).unwrap();

    let mut ceremony = Scripted::new();
    let mut db = fotwd::open_library_with(&f.root, &f.store, &mut ceremony, KDF_FOR_TESTS)
        .expect("an existing library refused to open for enrolment");
    assert_eq!(
        meeting_titles(&mut db),
        vec!["Quarterly review with Priya".to_owned()]
    );
    drop(db);

    assert!(f.blob_path().exists(), "the library was not enrolled");
    assert_eq!(ceremony.reveals, 1, "the key was not shown");

    // The master key is untouched, which is what "no re-encryption" means.
    assert!(
        stored_key_before.ct_eq(&f.store.get(SecretKey::DbMasterKey).unwrap()),
        "enrolment changed the master key, so the library was re-encrypted"
    );

    // And the new Recovery Key opens the *same* ciphertext.
    let key = ceremony.key().to_owned();
    f.store.delete(SecretKey::DbMasterKey).unwrap();
    let mut db = recover(&f.root, &f.store, &key).unwrap().db;
    assert_eq!(
        meeting_titles(&mut db),
        vec!["Quarterly review with Priya".to_owned()]
    );
    drop(db);

    // The database pages themselves were never rewritten by enrolment. (The
    // WAL moves as rows are read, so this compares the main file's first page,
    // which carries the SQLCipher salt and would change under a rekey.)
    let after = std::fs::read(f.db_path()).unwrap();
    assert_eq!(
        &before[..16],
        &after[..16],
        "the database header changed, so the library was re-keyed"
    );
}

/// Rotating the Recovery Key rewrites 200 bytes and nothing else.
#[test]
fn rotating_the_recovery_key_leaves_the_database_alone() {
    let f = Fixture::new();
    let ceremony = f.create();
    let old_key = ceremony.key().to_owned();
    let db_before = std::fs::read(f.db_path()).unwrap();

    // Rotation, as the settings screen would do it: unwrap with the old key,
    // re-wrap the same master key under a new one.
    let blob = WrappedMasterKey::read_from(&f.blob_path()).unwrap();
    let master = blob
        .unwrap_master(&RecoveryKey::parse(&old_key).unwrap(), &f.blob_path())
        .unwrap();
    let new_rk = RecoveryKey::generate().unwrap();
    WrappedMasterKey::wrap(&master, &new_rk, KDF_FOR_TESTS)
        .unwrap()
        .write_to(&f.blob_path())
        .unwrap();
    let new_key = new_rk.display_string().unwrap().expose().to_owned();

    assert_eq!(
        db_before,
        std::fs::read(f.db_path()).unwrap(),
        "rotating the Recovery Key rewrote the database"
    );

    f.store.delete(SecretKey::DbMasterKey).unwrap();
    assert!(
        recover(&f.root, &f.store, &old_key).is_err(),
        "the old Recovery Key still works after rotation"
    );
    let mut db = recover(&f.root, &f.store, &new_key)
        .expect("the new Recovery Key does not open the library")
        .db;
    assert_eq!(
        meeting_titles(&mut db),
        vec!["Quarterly review with Priya".to_owned()]
    );
}

// -------------------------------------------------------------- KEY-01 again

/// Neither key may reach the disk. The sealed file holds the master key
/// *encrypted*; if it ever held either secret in the clear, the whole feature
/// would be a plaintext key sitting next to the database it opens.
#[test]
fn neither_the_recovery_key_nor_the_master_key_is_written_anywhere() {
    let f = Fixture::new();
    let ceremony = f.create();
    let recovery_text = ceremony.key().to_owned();

    let rk = RecoveryKey::parse(&recovery_text).unwrap();
    let master = WrappedMasterKey::read_from(&f.blob_path())
        .unwrap()
        .unwrap_master(&rk, &f.blob_path())
        .unwrap();

    let master_hex: String = master
        .expose()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let rk_hex: String = rk.expose().iter().map(|b| format!("{b:02x}")).collect();
    let recovery_flat = recovery_text.replace('-', "");

    let files = f.all_bytes();
    assert!(files.len() >= 2, "only {} files to scan", files.len());

    for (path, bytes) in &files {
        for (label, needle) in [
            ("the Recovery Key as displayed", recovery_text.as_bytes()),
            ("the Recovery Key without dashes", recovery_flat.as_bytes()),
            ("the Recovery Key bytes", rk.expose().as_slice()),
            ("the Recovery Key in hex", rk_hex.as_bytes()),
            ("the master key bytes", master.expose().as_slice()),
            ("the master key in hex", master_hex.as_bytes()),
        ] {
            assert!(
                !contains(bytes, needle),
                "{label} was found in {}",
                path.display()
            );
        }
    }

    // Positive control: the scan finds what is really there, so its silence
    // above means something. The sealed ciphertext *is* in the file.
    let sealed_present = files
        .iter()
        .any(|(p, b)| p == &f.blob_path() && contains(b, b"sealed_key"));
    assert!(
        sealed_present,
        "the scan did not even see the sealed file, so it proves nothing"
    );
}

/// The generated master key is not a constant, and not derived from the
/// Recovery Key. A `wrap` that ignored its input would still pass every round
/// trip above.
#[test]
fn two_libraries_get_different_keys() {
    let a = Fixture::new();
    let b = Fixture::new();
    let ka = a.create();
    let kb = b.create();
    assert_ne!(ka.key(), kb.key(), "two installs got the same Recovery Key");

    let unwrap = |f: &Fixture, text: &str| -> MasterKeyBytes {
        WrappedMasterKey::read_from(&f.blob_path())
            .unwrap()
            .unwrap_master(&RecoveryKey::parse(text).unwrap(), &f.blob_path())
            .unwrap()
    };
    assert!(
        !unwrap(&a, ka.key()).ct_eq(&unwrap(&b, kb.key())),
        "two installs got the same master key"
    );

    // Cross-recovery must fail: b's key must not open a's library.
    a.store.delete(SecretKey::DbMasterKey).unwrap();
    assert!(recover(&a.root, &a.store, kb.key()).is_err());
}

/// A `db.sqlite3` from one install beside a `db.sqlite3.recovery` from another
/// — what happens when somebody reassembles a backup by hand. The sealed file
/// opens, the key inside it does not open the database, and that has to be its
/// own message: neither "wrong Recovery Key" nor "corrupt database" describes
/// what went wrong, and both send the user somewhere useless.
///
/// This also pins the ordering inside `recover`: the database is opened
/// **before** the keychain is written. The other order would leave the machine
/// holding a master key that opens nothing, and the next ordinary run would
/// then sail past the "no key here" guard and hand SQLCipher the wrong key.
#[test]
fn a_recovery_file_from_a_different_library_is_reported_as_a_mismatch() {
    let a = Fixture::new();
    let b = Fixture::new();
    a.create();
    let kb = b.create();

    // b's sealed file, a's database.
    std::fs::copy(b.blob_path(), a.blob_path()).unwrap();
    a.store.delete(SecretKey::DbMasterKey).unwrap();

    let err = recover(&a.root, &a.store, kb.key())
        .expect_err("a foreign recovery file opened this library");
    let text = err.to_string();

    assert!(!err.is_wrong_key(), "misreported as a wrong key: {err:?}");
    assert!(!err.is_malformed(), "misreported as a typo: {err:?}");
    assert!(
        text.contains("do not belong together"),
        "the message does not name the real problem: {text}"
    );
    assert!(
        !a.store.contains(SecretKey::DbMasterKey).unwrap(),
        "a key that opens nothing was written to the keychain"
    );

    // `check` has to answer the same question. A check that only proved the
    // sealed file unwraps would say "yes, that key is good" here — and the
    // user would find out otherwise on the day they needed it.
    let err = fotwd::recovery::check(&a.root, kb.key())
        .expect_err("--check passed a key that does not open this library");
    assert!(
        err.to_string().contains("do not belong together"),
        "check() did not test the pairing: {err}"
    );
}

/// Verifying a written-down key must not need the keychain and must not change
/// anything — this is what a user does a month later to check the card.
#[test]
fn checking_a_recovery_key_touches_nothing() {
    let f = Fixture::new();
    let ceremony = f.create();
    let before = std::fs::read(f.blob_path()).unwrap();

    fotwd::recovery::check(&f.root, ceremony.key()).expect("a valid key failed its own check");
    assert!(
        fotwd::recovery::check(&f.root, "fotw1-qqqq-qqqq-qqqq-qqqq-qqqq-qqqq-qqqq-qqqq").is_err()
    );

    assert_eq!(before, std::fs::read(f.blob_path()).unwrap());
    assert!(
        f.store.contains(SecretKey::DbMasterKey).unwrap(),
        "check() disturbed the keychain"
    );
}

/// `CeremonyError` has to be distinguishable, or `open_library` cannot write
/// different copy for "you declined" and "there is nobody here".
#[test]
fn ceremony_failures_are_distinguishable() {
    let declined = CeremonyError::Declined("no".to_owned());
    let no_human = CeremonyError::NoHuman("no tty".to_owned());
    assert!(declined.is_declined());
    assert!(!declined.is_no_human());
    assert!(no_human.is_no_human());
    assert!(!no_human.is_declined());
}
