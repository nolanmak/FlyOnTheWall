//! The [`KeyStore`] seam and its two backends.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::{SecretKey, SecretString, SecretsError};

/// Somewhere secrets can be kept.
///
/// Two implementations: [`OsKeyStore`] (the real one, backed by the platform
/// credential store) and [`InMemoryKeyStore`] (tests). There is deliberately
/// **no file-backed implementation** — see KEY-05 and [`OsKeyStore`]. If one
/// is ever added, this doc comment is the place the argument for it has to be
/// written down and lost.
///
/// Object-safe: the pipeline holds a `Box<dyn KeyStore>` so the same code path
/// runs against the fake in CI and the keychain in production.
pub trait KeyStore: Send + Sync {
    /// Store `secret` under `key`, replacing any existing value.
    fn set(&self, key: SecretKey, secret: &SecretString) -> Result<(), SecretsError>;

    /// Read the secret stored under `key`.
    ///
    /// Returns [`SecretsError::NotFound`] if it was never set. Read on demand
    /// and let the result drop — docs/REQUIREMENTS.md 10 requires keys are
    /// "read on demand into a `SecretString` and zeroized on drop; never held
    /// in a global".
    fn get(&self, key: SecretKey) -> Result<SecretString, SecretsError>;

    /// Remove the secret stored under `key`.
    ///
    /// Removing something that is not there succeeds: callers reset a provider
    /// without checking first, and a spurious error there only teaches them to
    /// ignore the result.
    fn delete(&self, key: SecretKey) -> Result<(), SecretsError>;

    /// Whether a secret is stored under `key`.
    fn contains(&self, key: SecretKey) -> Result<bool, SecretsError>;
}

/// Whether the platform credential store can be used, and if not, why.
///
/// Exists as a type so [`OsKeyStore::with_probe`] can be driven from a test: a
/// hosted CI runner cannot be made to *not* have a keychain on demand, so the
/// KEY-05 failure path would otherwise be the one path that never runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreAvailability {
    /// A credential store initialised successfully.
    Available,
    /// No usable credential store, with the platform's explanation.
    Unavailable(String),
}

/// The OS keychain: macOS Keychain Services, Windows Credential Manager,
/// Secret Service on Linux.
///
/// # KEY-05: this type refuses to exist rather than degrade
///
/// [`OsKeyStore::new`] probes the platform store *before* returning a value.
/// With no Secret Service it returns [`SecretsError::NoSecretService`] and no
/// `OsKeyStore` is constructed — so there is no object on which a caller could
/// invoke `set`, and therefore no code path from "no keychain" to "key written
/// somewhere else".
///
/// That structure is the point. The spec names Electron's `safeStorage` as the
/// anti-pattern: it hands back a working-looking object that encrypts with a
/// **hardcoded plaintext password**, and reports the degradation only through
/// a separate `getSelectedStorageBackend() === 'basic_text'` call. Every
/// caller who forgets that second call ships plaintext secrets and believes
/// otherwise. A control you have to remember to ask about is not a control; a
/// constructor that fails is.
///
/// `Debug` is safe to derive here precisely because the type is stateless: it
/// holds no handle, no service name, and above all no material.
#[derive(Debug)]
pub struct OsKeyStore {
    /// No state. The field exists so the struct cannot be built by a literal
    /// outside `with_probe`, which is what keeps the KEY-05 probe
    /// unskippable.
    _probe_passed: (),
}

impl OsKeyStore {
    /// Open the platform credential store, or fail.
    ///
    /// # Errors
    ///
    /// [`SecretsError::NoSecretService`] when no credential store is
    /// available. There is no third outcome and no fallback.
    pub fn new() -> Result<Self, SecretsError> {
        Self::with_probe(platform_availability)
    }

    /// Open the store using a caller-supplied availability probe.
    ///
    /// The seam that makes the KEY-05 failure path testable on a machine that
    /// does have a keychain. Production goes through [`OsKeyStore::new`].
    ///
    /// # Errors
    ///
    /// [`SecretsError::NoSecretService`] when the probe reports
    /// [`StoreAvailability::Unavailable`].
    pub fn with_probe(probe: impl FnOnce() -> StoreAvailability) -> Result<Self, SecretsError> {
        match probe() {
            StoreAvailability::Available => Ok(Self { _probe_passed: () }),
            StoreAvailability::Unavailable(why) => Err(SecretsError::NoSecretService(why)),
        }
    }

    /// Build the keyring handle for a key.
    fn entry(
        &self,
        operation: &'static str,
        key: SecretKey,
    ) -> Result<keyring::Entry, SecretsError> {
        let account = key.account();
        keyring::Entry::new(key.service(), &account)
            .map_err(|err| SecretsError::from_keyring(operation, &account, err))
    }
}

/// Ask the platform whether a credential store initialised.
///
/// `keyring::Entry::store_status()` is why this crate is on keyring 4 rather
/// than the flatter v3 API: it reports initialisation *without* attempting a
/// read or a write. A probe that had to write would either leave a test
/// credential in the user's keychain or, worse, discover the problem halfway
/// through a real `set()` — at which point we would have to guess whether a
/// partial credential was left behind.
fn platform_availability() -> StoreAvailability {
    match keyring::Entry::store_status() {
        Ok(()) => StoreAvailability::Available,
        Err(err) => StoreAvailability::Unavailable(err.to_string()),
    }
}

impl KeyStore for OsKeyStore {
    fn set(&self, key: SecretKey, secret: &SecretString) -> Result<(), SecretsError> {
        if secret.is_empty() {
            return Err(SecretsError::InvalidKeyMaterial(
                "refusing to store an empty secret".to_owned(),
            ));
        }
        let account = key.account();
        self.entry("writing", key)?
            .set_password(secret.expose())
            .map_err(|err| SecretsError::from_keyring("writing", &account, err))
    }

    fn get(&self, key: SecretKey) -> Result<SecretString, SecretsError> {
        let account = key.account();
        // `get_password` hands back a bare `String` — a copy of the material
        // outside `SecretString`'s protection. Wrap it in the same expression
        // so it is never bound to a name that could outlive this line, and so
        // the buffer is zeroed when the `SecretString` drops.
        keyring::Entry::new(key.service(), &account)
            .and_then(|entry| entry.get_password())
            .map(SecretString::new)
            .map_err(|err| SecretsError::from_keyring("reading", &account, err))
    }

    fn delete(&self, key: SecretKey) -> Result<(), SecretsError> {
        let account = key.account();
        match self.entry("deleting", key)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(SecretsError::from_keyring("deleting", &account, err)),
        }
    }

    fn contains(&self, key: SecretKey) -> Result<bool, SecretsError> {
        // There is no existence check in the keyring API that does not read
        // the credential, so this materialises the secret and immediately
        // drops it — zeroing the buffer on the way out. Prefer `get` when you
        // are going to need the value anyway; this is for the settings screen,
        // which only needs the boolean.
        match self.get(key) {
            Ok(_) => Ok(true),
            Err(err) if err.is_not_found() => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// A [`KeyStore`] that keeps secrets in memory and touches nothing else.
///
/// Carries the behavioural coverage for the whole trait, because the OS
/// backends cannot run on a CI box with no keychain and no D-Bus. It is also
/// what the KEY-01 acceptance test writes through: a store that provably never
/// opens a file is the right control for a test asserting no file contains a
/// key.
#[derive(Default)]
pub struct InMemoryKeyStore {
    entries: Mutex<BTreeMap<SecretKey, SecretString>>,
}

impl InMemoryKeyStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the map, ignoring poisoning.
    ///
    /// A panic in another thread while holding this lock cannot have left the
    /// map inconsistent — every operation is a single map call — and refusing
    /// to serve keys afterwards would turn an unrelated panic into a failed
    /// recording.
    fn entries(&self) -> std::sync::MutexGuard<'_, BTreeMap<SecretKey, SecretString>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl KeyStore for InMemoryKeyStore {
    fn set(&self, key: SecretKey, secret: &SecretString) -> Result<(), SecretsError> {
        if secret.is_empty() {
            return Err(SecretsError::InvalidKeyMaterial(
                "refusing to store an empty secret".to_owned(),
            ));
        }
        self.entries().insert(key, secret.clone());
        Ok(())
    }

    fn get(&self, key: SecretKey) -> Result<SecretString, SecretsError> {
        self.entries()
            .get(&key)
            .cloned()
            .ok_or_else(|| SecretsError::NotFound { key: key.account() })
    }

    fn delete(&self, key: SecretKey) -> Result<(), SecretsError> {
        self.entries().remove(&key);
        Ok(())
    }

    fn contains(&self, key: SecretKey) -> Result<bool, SecretsError> {
        Ok(self.entries().contains_key(&key))
    }
}

/// Whether tests that touch the real OS keychain should run.
///
/// Off unless `FOTW_KEYCHAIN_TESTS=1`. `cargo test --workspace` has to pass on
/// a hosted runner with no keychain, no D-Bus session and no secrets
/// (docs/REQUIREMENTS.md 5.6), and on macOS an unsigned test binary writing to
/// the login keychain raises an interactive unlock prompt that would hang CI
/// rather than fail it.
#[must_use]
pub fn os_tests_enabled() -> bool {
    std::env::var("FOTW_KEYCHAIN_TESTS").is_ok_and(|value| value == "1")
}

#[cfg(test)]
mod tests {
    use crate::{
        InMemoryKeyStore, KeyStore, OsKeyStore, Provider, SecretKey, SecretString, SecretsError,
        StoreAvailability, os_tests_enabled,
    };

    // ---------------------------------------------------------------- seam

    /// The pipeline holds a `Box<dyn KeyStore>` so it can be handed the
    /// in-memory backend under test and the OS backend in production. If the
    /// trait stops being object-safe this fails to compile, which is the
    /// point.
    #[test]
    fn keystore_is_object_safe() {
        let store: Box<dyn KeyStore> = Box::new(InMemoryKeyStore::new());
        assert!(!store.contains(SecretKey::DbMasterKey).unwrap());
    }

    // ------------------------------------------------------------ in-memory

    #[test]
    fn round_trips_every_known_key() {
        let store = InMemoryKeyStore::new();

        for key in SecretKey::ALL {
            let material = format!("material-for-{}", key.account());
            store
                .set(key, &SecretString::new(material.clone()))
                .unwrap();
            assert!(store.contains(key).unwrap());
            assert_eq!(store.get(key).unwrap().expose(), material);
        }
    }

    #[test]
    fn get_of_an_unset_key_is_not_found() {
        let store = InMemoryKeyStore::new();
        let err = store
            .get(SecretKey::ApiKey(Provider::Deepgram))
            .unwrap_err();
        assert!(
            matches!(err, SecretsError::NotFound { .. }),
            "expected NotFound, got {err:?}"
        );
        assert!(err.is_not_found());
        // The error names the key, never the key material.
        assert!(err.to_string().contains("apikey:deepgram"));
    }

    #[test]
    fn set_overwrites_and_delete_removes() {
        let store = InMemoryKeyStore::new();
        let key = SecretKey::ApiKey(Provider::OpenAi);

        store.set(key, &SecretString::new("first")).unwrap();
        store.set(key, &SecretString::new("second")).unwrap();
        assert_eq!(store.get(key).unwrap().expose(), "second");

        store.delete(key).unwrap();
        assert!(!store.contains(key).unwrap());
        assert!(store.get(key).unwrap_err().is_not_found());

        // Deleting what is not there is not an error: callers reset a
        // provider without first checking, and a spurious failure there just
        // teaches them to ignore the result.
        store.delete(key).unwrap();
    }

    #[test]
    fn keys_are_isolated_from_each_other() {
        let store = InMemoryKeyStore::new();
        store
            .set(
                SecretKey::ApiKey(Provider::Deepgram),
                &SecretString::new("dg"),
            )
            .unwrap();
        store
            .set(
                SecretKey::ApiKey(Provider::Anthropic),
                &SecretString::new("an"),
            )
            .unwrap();

        assert_eq!(
            store
                .get(SecretKey::ApiKey(Provider::Deepgram))
                .unwrap()
                .expose(),
            "dg"
        );
        assert!(!store.contains(SecretKey::ApiKey(Provider::OpenAi)).unwrap());
    }

    // ------------------------------------------------- KEY-05: no fallback

    /// KEY-05. With no secret service, construction fails and no store
    /// exists to write through. Contrast Electron's `safeStorage`, which
    /// silently swaps in a hardcoded plaintext password and reports it only
    /// via a getter nobody calls.
    #[test]
    fn os_store_refuses_to_construct_without_a_secret_service() {
        let result = OsKeyStore::with_probe(|| {
            StoreAvailability::Unavailable("no D-Bus session bus".to_owned())
        });

        let err = result.expect_err("constructed a key store with no secret service");
        assert!(
            matches!(err, SecretsError::NoSecretService(_)),
            "expected NoSecretService, got {err:?}"
        );
        assert!(err.is_no_secret_service());
        assert!(
            err.to_string().contains("no D-Bus session bus"),
            "the user cannot fix what we will not name: {err}"
        );
    }

    /// The failure has to be *loud*, not a degraded object. There is no
    /// `OsKeyStore` value to call `set` on, so there is no code path from a
    /// missing secret service to a plaintext write. This test asserts the
    /// consequence: the error type carries no store.
    #[test]
    fn a_failed_probe_yields_no_store_at_all() {
        let probed = OsKeyStore::with_probe(|| StoreAvailability::Unavailable("locked".to_owned()));
        assert!(probed.is_err());

        // And the happy path does yield one, so the test above is not passing
        // because `with_probe` always fails.
        let available = OsKeyStore::with_probe(|| StoreAvailability::Available);
        assert!(available.is_ok(), "an available probe must produce a store");
    }

    /// On a real machine `new()` may legitimately go either way — a developer
    /// laptop has a keychain, a hosted CI runner does not. What is *not*
    /// allowed is any third outcome: no silent success with a degraded
    /// backend, and no error that is not the one the user can act on.
    #[test]
    fn os_store_construction_either_succeeds_or_reports_no_secret_service() {
        match OsKeyStore::new() {
            Ok(_) => {}
            Err(err) => assert!(
                err.is_no_secret_service(),
                "construction failed for a reason the user cannot act on: {err:?}"
            ),
        }
    }

    // ------------------------------------------- opt-in real keychain test

    /// The OS backend against the real platform store. Skipped unless
    /// `FOTW_KEYCHAIN_TESTS=1`, because CI runners have no keychain and, on
    /// macOS, an unsigned test binary triggers an interactive unlock prompt
    /// that would hang the run.
    #[test]
    fn os_store_round_trips_against_the_real_keychain() {
        if !os_tests_enabled() {
            eprintln!("skipping: set FOTW_KEYCHAIN_TESTS=1 to exercise the OS keychain");
            return;
        }

        let store = OsKeyStore::new().expect("FOTW_KEYCHAIN_TESTS=1 but no secret service");
        let key = SecretKey::ApiKey(Provider::Deepgram);
        let material = "fotw-test-value-please-delete";

        store.set(key, &SecretString::new(material)).unwrap();
        assert!(store.contains(key).unwrap());
        assert_eq!(store.get(key).unwrap().expose(), material);

        store.delete(key).unwrap();
        assert!(!store.contains(key).unwrap());
    }
}
