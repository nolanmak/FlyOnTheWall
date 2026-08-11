//! Shared fixtures for the integration tests.
//!
//! Every test gets its own `TempDir` so the suite can run in parallel, and a
//! fixed key so a failing test can be reproduced by hand with the `sqlcipher`
//! CLI. A fixed key is safe here and only here — production keys come from the
//! OS keychain (docs/REQUIREMENTS.md 10).

#![allow(dead_code)]

use std::path::PathBuf;

use fotw_store::DbKey;
use tempfile::TempDir;

/// The one key the test suite uses. Deliberately not random: `x'0101..'` is
/// greppable, so a test that accidentally writes it somewhere shows up.
pub fn test_key() -> DbKey {
    DbKey::from_bytes([0x01; 32])
}

/// A fresh temp root with the canonical `<root>/db.sqlite3` layout from §9.2.
///
/// The `TempDir` is returned so the caller keeps it alive; dropping it deletes
/// the directory out from under the connection.
pub fn tmp_db() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db.sqlite3");
    (dir, path)
}
