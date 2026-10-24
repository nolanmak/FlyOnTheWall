//! Consumer-visible contracts shared by both apps. Uses fixture keys and memory only.
use crate::{
    InMemoryKeyStore, KEYRING_SERVICE, KeyStore, Provider, Redactor, SecretKey, SecretString,
};

/// Verify stable credential addressing, isolation, replacement and redaction.
///
/// Consumers call this in their own integration suite after resolving their
/// pinned dependency. It must never create an OS keychain or inspect user data.
///
/// # Panics
/// If a shared contract has changed or a key can escape through formatting/logs.
pub fn credentials() {
    assert_eq!(KEYRING_SERVICE, "com.flyonthewall.fotw");
    let key = SecretKey::ApiKey(Provider::Deepgram);
    assert_eq!(key.account(), "apikey:deepgram");
    let store = InMemoryKeyStore::new();
    let original = SecretString::new("fixture-original-do-not-use");
    let replacement = SecretString::new("fixture-replacement-do-not-use");
    assert!(store.get(key).is_err());
    store.set(key, &original).unwrap();
    store.set(SecretKey::DbMasterKey, &replacement).unwrap();
    assert_eq!(store.get(key).unwrap().expose(), original.expose());
    assert_eq!(
        store.get(SecretKey::DbMasterKey).unwrap().expose(),
        replacement.expose()
    );
    let rendered = format!("{original} {original:?} {:?}", store.get(key));
    assert!(!rendered.contains(original.expose()));
    let redactor = Redactor::new();
    redactor.register(&original);
    let logged = redactor.redact(&format!("retained-context {}", original.expose()));
    assert!(logged.contains("retained-context"));
    assert!(!logged.contains(original.expose()));
    assert!(
        !redactor
            .redact_field("Authorization", "Bearer fixture-token")
            .contains("fixture-token")
    );
    assert!(store.set(key, &SecretString::new("")).is_err());
    assert_eq!(store.get(key).unwrap().expose(), original.expose());
    store.set(key, &replacement).unwrap();
    assert_eq!(store.get(key).unwrap().expose(), replacement.expose());
    store.delete(key).unwrap();
    assert!(store.get(key).is_err());
    assert_eq!(
        store.get(SecretKey::DbMasterKey).unwrap().expose(),
        replacement.expose()
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_consumer_credentials() {
        super::credentials();
    }
}
