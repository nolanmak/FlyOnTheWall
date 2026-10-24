//! Linux-only migration probe, run by scripts/test-keyring-interop.sh.

#[cfg(test)]
mod tests {
    use keyring_core::api::CredentialStoreApi;

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
        let new = replacement
            .build(&service, "synthetic-master-key", None)
            .unwrap();
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
