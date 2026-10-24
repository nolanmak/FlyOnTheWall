//! Explicit stores preserve credential addresses without a global default store.
use std::sync::{Arc, LazyLock};

use keyring_core::{CredentialStore, Entry, Error, Result};

static STORE: LazyLock<Result<Arc<CredentialStore>>> = LazyLock::new(initialize);

pub(crate) fn store_status() -> std::result::Result<(), &'static Error> {
    STORE.as_ref().map(|_| ())
}

pub(crate) fn entry(service: &str, account: &str) -> Result<Entry> {
    STORE
        .as_ref()
        .map_err(|_| Error::NoDefaultStore)?
        .build(service, account, None)
}

fn initialize() -> Result<Arc<CredentialStore>> {
    #[cfg(target_os = "macos")]
    {
        Ok(apple_native_keyring_store::keychain::Store::new()?)
    }
    #[cfg(target_os = "windows")]
    {
        Ok(windows_native_keyring_store::Store::new()?)
    }
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ))]
    {
        Ok(dbus_secret_service_keyring_store::Store::new()?)
    }
    #[cfg(not(all(any(unix, windows), not(any(target_os = "ios", target_os = "android")))))]
    Err(Error::NoDefaultStore)
}
