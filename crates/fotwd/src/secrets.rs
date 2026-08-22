//! Where the daemon gets its keys.
//!
//! Two secrets matter here: the database master key, which encrypts the whole
//! meeting library, and the provider API key. Both come from the OS keychain
//! and neither is ever written to disk, passed as an argument, or held in a
//! global (§10).
//!
//! # The master key is generated once, and only behind the ceremony
//!
//! On first run a 32-byte key is drawn from the OS CSPRNG and stored in the
//! keychain. Losing it without the Recovery Key means permanent data loss,
//! which is why §10 makes the recovery-key dialog unskippable.
//!
//! This module deliberately **does not** generate-and-store in one call any
//! more. It offers [`generate_master_key`] and [`store_master_key`] separately,
//! so the first-run ceremony in [`crate::recovery`] sits between them: a key is
//! only ever committed to the keychain after the user has been shown a Recovery
//! Key for it and typed part of it back. A single `db_key()` that minted and
//! stored in one step made that ordering impossible to enforce, and the
//! warning it printed named a backup the user had no way to take.
//!
//! # Why an environment variable is still accepted for the provider key
//!
//! `DEEPGRAM_API_KEY` remains a fallback for headless and CI use, where there
//! is no keychain to unlock. It is *not* the recommended path: an environment
//! variable is readable by every child process and shows up in a crash dump.
//! The keychain is tried first and the fallback says so out loud.

use std::sync::OnceLock;

use fotw_secrets::recovery::MasterKeyBytes;
use fotw_secrets::{
    CachedKeyStore, KeyStore, OsKeyStore, Provider, SecretKey, SecretString, SecretsError,
};
use fotw_store::DbKey;

/// How the daemon resolved a *provider* secret, so the UI can tell the user.
///
/// There used to be a `Generated` variant here, for the master key minted on
/// first run. It is gone with `db_key()`: the master key's provenance is now a
/// decision [`crate::open_library_with`] makes from three explicit states, and
/// a variant that nothing can ever produce is a variant that misleads the next
/// person to read this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Read from the OS keychain — the supported path.
    Keychain,
    /// Read from the environment. Works, but readable by any child process.
    Environment,
}

/// Re-exported so callers and tests name one deadline.
///
/// The guard itself lives on `OsKeyStore`, not here: a timeout that only wraps
/// the two call sites this module happens to remember is a timeout the next
/// call site will not have.
pub use fotw_secrets::KEYCHAIN_TIMEOUT;

/// Open the OS keychain, or explain why it is unavailable.
///
/// Never degrades to a plaintext store. On Linux with no Secret Service this
/// is a hard failure by design: silently writing keys to a file would be a
/// headline-grade defect for a project whose pitch is that your keys stay
/// yours.
/// # Why one store for the whole process
///
/// The returned store caches what the keychain already told it, and that is
/// only worth anything if every caller shares one. `open_library` reads
/// `db:masterkey` on every call — `list`, `serve`, `summarize`, persist,
/// retention, import and export all go through it — and on macOS each read of
/// an item whose ACL does not list this exact binary is a separate approval
/// dialog. Handing out a fresh store per call would mean a fresh empty cache
/// per call, which is where six dialogs in a row came from.
///
/// This is the shape Chromium uses for the `<App> Safe Storage` item every
/// Electron app keeps in the keychain: read once, hold for the process.
pub fn keystore() -> Result<&'static CachedKeyStore<OsKeyStore>, SecretsError> {
    static STORE: OnceLock<Result<CachedKeyStore<OsKeyStore>, String>> = OnceLock::new();

    // The error is kept as a string because `SecretsError` is not `Clone` and
    // a `OnceLock` hands out shared references. The text is what the caller
    // prints anyway.
    match STORE.get_or_init(|| {
        OsKeyStore::new()
            .map(CachedKeyStore::new)
            .map_err(|e| e.to_string())
    }) {
        Ok(store) => Ok(store),
        Err(why) => Err(SecretsError::NoSecretService(why.clone())),
    }
}

/// The stored database master key, or `None` if this machine has never had one.
///
/// `None` is a *state*, not an error: on a first run there is no key yet, and
/// on a machine that has lost its keychain there is no key any more. Those two
/// need completely different handling — one runs the first-run ceremony, the
/// other must refuse to touch the library and point at `fotwd recover` — and
/// the caller can only tell them apart if this returns without deciding.
///
/// # Errors
///
/// Anything other than "not stored": a locked keychain, an ACL mismatch, a
/// stalled call. Those must not be mistaken for "no key", because mistaking
/// them means minting a second key over a perfectly good library.
pub fn load_master_key(store: &dyn KeyStore) -> Result<Option<MasterKeyBytes>, SecretsError> {
    match store.get(SecretKey::DbMasterKey) {
        Ok(secret) => {
            let bytes = decode_hex(secret.expose()).ok_or_else(|| {
                SecretsError::InvalidKeyMaterial(
                    "the stored database key is not 64 hex characters; refusing to \
                     guess, because opening with the wrong key is indistinguishable \
                     from a corrupt file"
                        .to_owned(),
                )
            })?;
            Ok(Some(MasterKeyBytes::new(bytes)))
        }
        Err(SecretsError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A fresh 32-byte master key, **not** stored anywhere.
///
/// Split from [`store_master_key`] so the recovery-key ceremony can run between
/// the two. Nothing is committed until the user has a Recovery Key in hand.
///
/// # Errors
///
/// Propagates a CSPRNG failure rather than falling back to a weaker source.
pub fn generate_master_key() -> Result<MasterKeyBytes, SecretsError> {
    MasterKeyBytes::generate()
        .map_err(|e| SecretsError::InvalidKeyMaterial(format!("could not generate a key: {e}")))
}

/// Commit a master key to the keychain.
///
/// # Errors
///
/// Whatever the platform credential store says. There is no file fallback.
pub fn store_master_key(store: &dyn KeyStore, key: &MasterKeyBytes) -> Result<(), SecretsError> {
    store.set(
        SecretKey::DbMasterKey,
        &SecretString::new(encode_hex(key.expose())),
    )
}

/// The `PRAGMA key` form of a master key.
///
/// # Errors
///
/// [`SecretsError::InvalidKeyMaterial`] if the material is not 32 bytes —
/// SQLCipher would silently zero-pad it into a weaker database that still
/// opens.
pub fn db_key_of(key: &MasterKeyBytes) -> Result<DbKey, SecretsError> {
    DbKey::from_slice(key.expose()).map_err(|e| SecretsError::InvalidKeyMaterial(format!("{e}")))
}

/// The Deepgram API key, from the keychain or the environment.
pub fn deepgram_key(store: &dyn KeyStore) -> Option<(SecretString, Origin)> {
    match store.get(SecretKey::ApiKey(Provider::Deepgram)) {
        Ok(secret) => return Some((secret, Origin::Keychain)),
        Err(e @ SecretsError::Platform { .. }) => {
            // Say it out loud rather than falling through to the environment:
            // silently using a different key source after a keychain stall is
            // how a user ends up debugging the wrong thing.
            eprintln!("  ! keychain: {e}");
        }
        Err(_) => {}
    }
    match std::env::var("DEEPGRAM_API_KEY") {
        Ok(v) if !v.trim().is_empty() => Some((SecretString::from_pasted(v), Origin::Environment)),
        _ => None,
    }
}

/// Store a pasted provider key in the keychain.
pub fn store_key(
    store: &dyn KeyStore,
    provider: Provider,
    material: &str,
) -> Result<(), SecretsError> {
    let secret = SecretString::from_pasted(material);
    if secret.expose().is_empty() {
        return Err(SecretsError::InvalidKeyMaterial(
            "the key is empty".to_owned(),
        ));
    }
    store.set(SecretKey::ApiKey(provider), &secret)
}

fn encode_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_secrets::InMemoryKeyStore;

    #[test]
    fn hex_round_trips() {
        let bytes = [0xabu8; 32];
        assert_eq!(decode_hex(&encode_hex(&bytes)), Some(bytes));
    }

    #[test]
    fn a_malformed_stored_key_is_refused_rather_than_guessed() {
        // Opening SQLCipher with the wrong key is indistinguishable from a
        // corrupt file, so a half-readable key must be an error here rather
        // than a mystery three layers down.
        let store = InMemoryKeyStore::new();
        store
            .set(SecretKey::DbMasterKey, &SecretString::new("not-hex"))
            .unwrap();
        assert!(load_master_key(&store).is_err());
    }

    /// The distinction the whole recovery path rests on: "there is no key"
    /// must not be reported the same way as "the keychain would not answer".
    /// The first runs onboarding; the second must never mint a second key over
    /// a library that already has one.
    #[test]
    fn an_absent_key_is_none_and_not_an_error() {
        let store = InMemoryKeyStore::new();
        assert!(load_master_key(&store).unwrap().is_none());
    }

    #[test]
    fn a_stored_key_round_trips_through_the_keychain() {
        let store = InMemoryKeyStore::new();
        let key = generate_master_key().unwrap();
        store_master_key(&store, &key).unwrap();

        let back = load_master_key(&store).unwrap().expect("key vanished");
        assert!(
            key.ct_eq(&back),
            "a second run must reuse the key, or every restart orphans the library"
        );
        assert_eq!(
            db_key_of(&key).unwrap().pragma_literal(),
            db_key_of(&back).unwrap().pragma_literal()
        );
    }

    #[test]
    fn generated_keys_are_not_all_the_same() {
        let a = generate_master_key().unwrap();
        let b = generate_master_key().unwrap();
        assert!(!a.ct_eq(&b), "the CSPRNG returned the same 32 bytes twice");
        assert!(a.expose().iter().any(|b| *b != 0), "key is all zeroes");
    }

    #[test]
    fn a_stored_provider_key_is_preferred_over_the_environment() {
        let store = InMemoryKeyStore::new();
        store_key(&store, Provider::Deepgram, "from-keychain").unwrap();
        let (secret, origin) = deepgram_key(&store).unwrap();
        assert_eq!(origin, Origin::Keychain);
        assert_eq!(secret.expose(), "from-keychain");
    }

    #[test]
    fn an_empty_key_is_rejected_before_it_reaches_the_keychain() {
        let store = InMemoryKeyStore::new();
        assert!(store_key(&store, Provider::Deepgram, "   ").is_err());
        assert!(
            !store
                .contains(SecretKey::ApiKey(Provider::Deepgram))
                .unwrap()
        );
    }
}
