//! `fotwd` — the FlyOnTheWall daemon.
//!
//! Owns capture, transcription and storage, and (later) serves the web UI on
//! loopback. Exposed as a library so the same session machinery is reachable
//! from tests and from the `fotw` CLI without duplicating the wiring.

#![warn(missing_docs)]

pub mod audit;
pub mod consent;
pub mod detect;
pub mod detection_preferences;
pub mod documents;
pub mod engine;
pub mod engine_control;
pub mod enrich;
pub mod failover;
pub mod github;
pub mod health;
pub mod journal;
#[cfg(feature = "experimental-aec")]
pub mod live_aec;
pub mod mcp;
mod model_capture;
pub mod model_setup;
pub mod model_setup_control;
pub mod native_shell;
pub mod okf;
pub mod onboard;
pub mod persist;
pub mod recording;
mod recording_limit;
pub mod recovery;
pub mod retention;
pub mod secrets;
pub mod serve;
pub mod session;
pub mod speechd;
pub mod stt;
pub mod summarize;
mod summary_guard;
/// Names the integration tests share, so none of them hand-writes one that
/// resolves to a real engine — #83. Absent from every shipped build; see the
/// `test-guards` feature in `Cargo.toml`.
#[cfg(feature = "test-guards")]
pub mod testing;
pub mod transport;

pub use session::{LegAudio, LegBuffers, SessionOutcome, Transcription};

use std::path::Path;

use fotw_secrets::KeyStore;
use fotw_secrets::recovery::{KdfParams, blob_path_for};
use fotw_store::Db;

use crate::recovery::{Ceremony, TtyCeremony};

/// Open the meeting library beside the sessions directory, keyed from the OS
/// keychain.
///
/// Shared by every command so the key path is written once. The master key is
/// generated on first run behind the Recovery Key ceremony, and reused after
/// that; it is never written to disk in the clear, never logged, and never
/// passed as an argument.
///
/// # Errors
///
/// A human-readable message, because every caller is a CLI command that prints
/// it. See [`open_library_with`] for the cases.
pub fn open_library(root: &Path) -> Result<Db, String> {
    let store = secrets::keystore().map_err(|e| {
        format!(
            "no OS keychain available: {e}\n  \
             FlyOnTheWall will not fall back to storing keys in a file."
        )
    })?;
    let data_root = root.parent().unwrap_or(root);
    let mut ceremony = TtyCeremony::new();
    open_library_with(data_root, store, &mut ceremony, KdfParams::default())
}

/// [`open_library`] with every dependency named.
///
/// The seam that makes the first-run path testable: a real keychain cannot be
/// emptied on demand and a real terminal cannot be scripted, so both are
/// arguments.
///
/// # The three states, and why the middle one is the whole point
///
/// | keychain | `db.sqlite3` | what happens |
/// |---|---|---|
/// | has the key | anything | open it; enrol a Recovery Key if there is not one yet |
/// | **no key** | **exists** | **refuse** — point at `fotwd recover` |
/// | no key | absent | first run: ceremony, then create |
///
/// The middle row is the reason this function exists in this shape. Generating
/// a fresh key there is the natural thing for the code to do and is a
/// catastrophe: SQLCipher would be handed a key that does not match the file
/// and would report *"file is not a database"* — a corruption message for a
/// library that is perfectly intact — and by the time anyone worked that out
/// they would have restored over it.
///
/// # Errors
///
/// A human-readable message. Refusals name the command that fixes them.
pub fn open_library_with(
    data_root: &Path,
    store: &dyn KeyStore,
    ceremony: &mut dyn Ceremony,
    kdf: KdfParams,
) -> Result<Db, String> {
    let db_path = data_root.join("db.sqlite3");
    let blob_path = blob_path_for(&db_path);

    match secrets::load_master_key(store).map_err(|e| format!("{e}"))? {
        Some(master) => {
            // A library from before issue #38 has a key and no sealed file. It
            // gains one here, and — because the Recovery Key *wraps* the master
            // key rather than replacing it — without a single page being
            // rewritten.
            if !blob_path.exists() {
                match recovery::enroll(ceremony, &master, &blob_path, kdf, false) {
                    Ok(()) => {}
                    // Not fatal, deliberately. Refusing to open an existing
                    // library because a daemon started under launchd cannot show
                    // a dialog would take a working install offline over a
                    // backup step. Loud, and retried on the next interactive run.
                    Err(e) => {
                        ceremony.tell("");
                        ceremony
                            .tell("  ! This library has NO Recovery Key. If the keychain entry is");
                        ceremony
                            .tell("    lost, it cannot be opened again by anyone, including us.");
                        ceremony.tell(&format!("    {e}"));
                        ceremony.tell("");
                    }
                }
            }
            let key = secrets::db_key_of(&master).map_err(|e| format!("{e}"))?;
            Db::open(&db_path, &key).map_err(|e| format!("{e}"))
        }

        None if db_path.exists() => Err(lost_key_message(&db_path, &blob_path)),

        None => {
            // Nothing is committed until the ceremony passes: not the key, not
            // the sealed file, and not the database — `Db::open` is what
            // creates `db.sqlite3`, and it runs last.
            let master = secrets::generate_master_key().map_err(|e| format!("{e}"))?;
            recovery::enroll(ceremony, &master, &blob_path, kdf, true)
                .map_err(|e| format!("{e}"))?;
            secrets::store_master_key(store, &master).map_err(|e| format!("{e}"))?;
            let key = secrets::db_key_of(&master).map_err(|e| format!("{e}"))?;
            Db::open(&db_path, &key).map_err(|e| format!("{e}"))
        }
    }
}

/// What to say when the library is here and its key is not.
fn lost_key_message(db_path: &Path, blob_path: &Path) -> String {
    let mut out = format!(
        "there is a library at {} but its master key is not in this machine's keychain.\n\n  \
         FlyOnTheWall will NOT generate a replacement. A new key would not open this\n  \
         file, and SQLCipher reports a wrong key as \"file is not a database\" — a\n  \
         corruption message for a library that is intact.\n\n",
        db_path.display()
    );
    if blob_path.exists() {
        out.push_str(&format!(
            "  Your library is recoverable. Run:\n\n      \
             fotwd recover\n\n  \
             and enter the Recovery Key you wrote down at first run. The sealed key\n  \
             it needs is already here ({}).\n",
            blob_path.display()
        ));
    } else {
        out.push_str(&format!(
            "  There is also no recovery file at {}. If you have the Recovery Key you\n  \
             wrote down, restore that file from a backup of this folder and run\n  \
             `fotwd recover`. Without either, this library cannot be opened — by\n  \
             anyone.\n",
            blob_path.display()
        ));
    }
    out
}

/// Content-free per-source capture health and session-scoped retry mailbox.
pub mod capture_health;

/// Dashboard transcription settings.
pub mod speech_control;

mod interrupted_capture;
mod raw_replay;
/// Owned saved-audio transcription recovery.
pub mod speech_recovery;
/// Content-free live transcription provenance.
pub mod speech_status;

pub mod capture_settings;

/// Default sessions location shared by the daemon and capture diagnostics.
#[must_use]
pub fn default_session_root() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(|home| {
            std::path::PathBuf::from(home)
                .join("Library/Application Support/com.flyonthewall.fotw/sessions")
        })
        .unwrap_or_else(|_| std::env::temp_dir().join("fotw-sessions"))
}
/// Lifetime ownership for a single daemon/native shell per library.
pub mod instance;
mod native_actions;
/// Nonblocking native command/status bridge and render admission.
pub mod native_bridge;
/// Bounded read-only activity sampling for native meeting offers.
pub mod native_detection;
/// Recorder-driven interaction model for the connected native shell.
pub mod native_host;
/// Native app launch selection and the AppKit/daemon doorway.
pub mod native_launch;
/// Capture and persistence shutdown observations.
pub mod recorder_lifecycle;

/// App-owned, network-isolated local summary inference.
#[cfg(unix)]
pub mod local_summary;
