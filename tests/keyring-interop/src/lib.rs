//! Linux-only migration probe, run by scripts/test-keyring-interop.sh.

// Compile the production adapter itself, without enabling real-account tests
// or exposing an OS-store hook in the public application API.
#[cfg(all(test, target_os = "linux"))]
#[path = "../../../crates/fotw-secrets/src/platform.rs"]
mod production_platform;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use keyring_core::api::CredentialStoreApi;

    #[test]
    fn unavailable_service_refuses_initialization_and_entry_creation() {
        assert_eq!(
            std::env::var("FOTW_ISOLATED_KEYRING_TEST").as_deref(),
            Ok("unavailable")
        );
        assert!(super::production_platform::store_status().is_err());
        assert!(matches!(
            super::production_platform::entry("org.flyonthewall.interop-test", "absent"),
            Err(keyring_core::Error::NoDefaultStore)
        ));
    }

    #[test]
    fn backends_share_existing_binary_credentials_without_copying_them() {
        assert!(cfg!(target_os = "linux"), "Run the isolated Linux script");
        assert_eq!(
            std::env::var("FOTW_ISOLATED_KEYRING_TEST").as_deref(),
            Ok("1")
        );
        let legacy = zbus_secret_service_keyring_store::Store::new().unwrap();
        let replacement = dbus_secret_service_keyring_store::Store::new().unwrap();
        // Neither backend may use the application's real service/account.
        let service = format!("org.flyonthewall.interop-test.{}", std::process::id());
        let old = legacy
            .build(&service, "synthetic-master-key", None)
            .unwrap();
        super::production_platform::store_status().unwrap();
        let new = super::production_platform::entry(&service, "synthetic-master-key").unwrap();
        let distinct = replacement
            .build(&service, "different-account", None)
            .unwrap();
        let initial: Vec<u8> = (0..=255).collect();
        old.set_secret(&initial).unwrap();
        assert_eq!(new.get_secret().unwrap(), initial);
        assert!(matches!(
            distinct.get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));

        new.set_secret(b"updated\0binary\xffsecret").unwrap();
        assert_eq!(old.get_secret().unwrap(), b"updated\0binary\xffsecret");
        // An update must change the existing item, not create an ambiguous copy.
        let query = std::collections::HashMap::from([
            ("service", service.as_str()),
            ("username", "synthetic-master-key"),
        ]);
        assert_eq!(replacement.search(&query).unwrap().len(), 1);
        assert_eq!(legacy.search(&query).unwrap().len(), 1);

        new.delete_credential().unwrap();
        assert!(matches!(
            old.get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));
        new.set_secret(b"created-by-replacement").unwrap();
        assert_eq!(old.get_secret().unwrap(), b"created-by-replacement");
        old.delete_credential().unwrap();
        assert!(matches!(
            new.get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));
    }
}
