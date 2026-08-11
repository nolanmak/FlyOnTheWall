//! `fotwd` — the FlyOnTheWall daemon.
//!
//! Owns capture, transcription and storage, and (later) serves the web UI on
//! loopback. Exposed as a library so the same session machinery is reachable
//! from tests and from the `fotw` CLI without duplicating the wiring.

#![warn(missing_docs)]

pub mod consent;
pub mod persist;
pub mod secrets;
pub mod serve;
pub mod session;
pub mod summarize;
pub mod transport;

pub use session::{SessionOutcome, Transcription};

/// Open the meeting library beside the sessions directory, keyed from the OS
/// keychain.
///
/// Shared by every command so the key path is written once. The master key is
/// generated on first run and reused; it is never written to disk, never
/// logged, and never passed as an argument.
pub fn open_library(root: &std::path::Path) -> Result<fotw_store::Db, String> {
    let store = secrets::keystore().map_err(|e| {
        format!(
            "no OS keychain available: {e}\n  \
             FlyOnTheWall will not fall back to storing keys in a file."
        )
    })?;
    let (key, origin) = secrets::db_key(&store).map_err(|e| format!("{e}"))?;
    if origin == secrets::Origin::Generated {
        eprintln!("  ! a new library key was generated and stored in your keychain.");
        eprintln!("    Back it up: losing it without a Recovery Key means the");
        eprintln!("    library cannot be opened again.");
    }
    let dir = root.parent().unwrap_or(root);
    fotw_store::Db::open(dir.join("db.sqlite3"), &key).map_err(|e| format!("{e}"))
}
