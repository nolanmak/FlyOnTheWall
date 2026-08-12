//! Where the daemon gets its keys.
//!
//! Two secrets matter here: the database master key, which encrypts the whole
//! meeting library, and the provider API key. Both come from the OS keychain
//! and neither is ever written to disk, passed as an argument, or held in a
//! global (§10).
//!
//! # The master key is generated once and never shown
//!
//! On first run a 32-byte key is drawn from the OS CSPRNG and stored in the
//! keychain. Losing it without the Recovery Key means permanent data loss,
//! which is why §10 makes the recovery-key dialog unskippable — that dialog
//! belongs to the UI and is not built yet, so this module deliberately fails
//! loudly rather than silently minting a key a user cannot back up.
//!
//! # Why an environment variable is still accepted for the provider key
//!
//! `DEEPGRAM_API_KEY` remains a fallback for headless and CI use, where there
//! is no keychain to unlock. It is *not* the recommended path: an environment
//! variable is readable by every child process and shows up in a crash dump.
//! The keychain is tried first and the fallback says so out loud.

use fotw_secrets::{KeyStore, OsKeyStore, Provider, SecretKey, SecretString, SecretsError};
use fotw_store::DbKey;

/// How the daemon resolved a secret, so the UI can tell the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Read from the OS keychain — the supported path.
    Keychain,
    /// Read from the environment. Works, but readable by any child process.
    Environment,
    /// Generated fresh and stored.
    Generated,
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
pub fn keystore() -> Result<OsKeyStore, SecretsError> {
    OsKeyStore::new()
}

/// The database master key, generated on first run.
///
/// Returns the key and how it was obtained, so the caller can prompt for a
/// Recovery Key the first time.
pub fn db_key(store: &dyn KeyStore) -> Result<(DbKey, Origin), SecretsError> {
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
            Ok((DbKey::from_bytes(bytes), Origin::Keychain))
        }
        Err(SecretsError::NotFound { .. }) => {
            let bytes = random_32();
            store.set(
                SecretKey::DbMasterKey,
                &SecretString::new(encode_hex(&bytes)),
            )?;
            Ok((DbKey::from_bytes(bytes), Origin::Generated))
        }
        Err(e) => Err(e),
    }
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

/// 32 bytes from the OS CSPRNG.
///
/// Read straight from `/dev/urandom` rather than taking a dependency: this is
/// the one place a weak source would be catastrophic and unnoticeable, and
/// the fewer layers between the OS and the key the better.
fn random_32() -> [u8; 32] {
    use std::io::Read;
    let mut buf = [0u8; 32];
    // A failure here means the OS has no entropy source, which is not a
    // condition to paper over with a fallback.
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("the operating system must provide /dev/urandom");
    buf
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
        assert!(db_key(&store).is_err());
    }

    #[test]
    fn the_master_key_is_generated_once_and_then_reused() {
        let store = InMemoryKeyStore::new();
        let (first, origin) = db_key(&store).unwrap();
        assert_eq!(origin, Origin::Generated);

        let (second, origin) = db_key(&store).unwrap();
        assert_eq!(origin, Origin::Keychain);
        assert_eq!(
            first.pragma_literal(),
            second.pragma_literal(),
            "a second run must reuse the key, or every restart orphans the library"
        );
    }

    #[test]
    fn generated_keys_are_not_all_the_same() {
        let a = random_32();
        let b = random_32();
        assert_ne!(a, b, "the CSPRNG returned the same 32 bytes twice");
        assert!(a.iter().any(|b| *b != 0), "key is all zeroes");
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
