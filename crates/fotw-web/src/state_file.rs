//! Where the `fotw` CLI finds the running daemon — ING-12.
//!
//! The file holds the ephemeral port and the per-start bearer token, which
//! together are complete access to every transcript in the library. §10.1 puts
//! same-user local malware out of scope (it can read the SQLCipher database
//! directly), but it does **not** put *other user accounts* out of scope: a
//! shared Mac, a family iMac, a lab machine. `~` is `drwxr-xr-x` by default on
//! macOS, so a file written there with a default umask is world-readable, and
//! another account would get the port and the token by reading it.
//!
//! Hence: the directory is `0700`, the file is `0600`, and the mode is set
//! **at creation** rather than afterwards. `File::create` then `set_permissions`
//! leaves a window — short, but a window — in which the file exists with the
//! umask's mode and already has the token in it. `OpenOptions::mode` closes
//! it.
//!
//! The write is to a temporary file in the same directory followed by
//! `rename(2)`, which is atomic within a filesystem. A reader therefore sees
//! either the old state or the new one, never a half-written token — which
//! would otherwise show up as the CLI failing to attach to a daemon that is
//! running perfectly.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What the CLI needs to talk to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
    /// The loopback port `axum` is listening on.
    pub port: u16,
    /// The per-start bearer token (ING-05). This is the secret the file mode
    /// exists to protect.
    pub token: String,
}

/// Write `state` to `path` atomically, with `0600` in a `0700` directory.
///
/// # Errors
///
/// Any filesystem failure. A caller that cannot write this file should not
/// come up: a daemon the CLI cannot find is a daemon the user cannot stop.
pub fn write_state_file(path: &Path, state: &DaemonState) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the state file needs a parent directory"))?;
    create_private_dir(dir)?;

    // Same directory, so the rename cannot cross a filesystem boundary — and
    // a cross-device rename is `EXDEV`, not a silent copy.
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state")
    ));
    let json = serde_json::to_vec_pretty(state)
        .map_err(|e| io::Error::other(format!("serialising the daemon state: {e}")))?;

    {
        use std::io::Write as _;
        let mut file = private_file(&tmp)?;
        file.write_all(&json)?;
        // Durable before the rename: otherwise a crash can leave the rename
        // committed and the contents empty, and the CLI reads a zero-length
        // file it will report as corruption.
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Read a state file back.
///
/// # Errors
///
/// The file is missing, unreadable or not the JSON this module writes.
pub fn read_state_file(path: &Path) -> io::Result<DaemonState> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::other(format!("parsing the daemon state: {e}")))
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        return Ok(());
    }
    // The directory may predate this code, or have been created by an
    // installer with a looser umask. Tighten it rather than trusting it: the
    // file mode below protects the contents, but a group-writable *directory*
    // lets another account replace the file wholesale and point the CLI at a
    // daemon of their choosing.
    let mode = fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        // Set at creation, not after. See the module docs.
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    // Windows ACLs are not mode bits and the equivalent is a different API
    // (`SetNamedSecurityInfo` with an explicit DACL). The daemon is macOS-first
    // and this crate builds on Linux for CI; a Windows port owes this function
    // a real implementation, and the compile-time split is where that debt is
    // recorded rather than a silently permissive fallback.
    fs::create_dir_all(dir)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> io::Result<fs::File> {
    fs::File::create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fotw-web-test-{}-{name}",
            crate::secret::random_token()
        ));
        dir.join("state.json")
    }

    #[test]
    fn it_round_trips() {
        let path = scratch("roundtrip");
        let state = DaemonState {
            port: 51234,
            token: "a".repeat(64),
        };
        write_state_file(&path, &state).unwrap();
        assert_eq!(read_state_file(&path).unwrap(), state);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_0600_in_a_0700_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = scratch("modes");
        write_state_file(
            &path,
            &DaemonState {
                port: 1,
                token: "t".into(),
            },
        )
        .unwrap();

        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "another account must not be able to read it"
        );
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A pre-existing world-readable directory is tightened, not accepted.
    #[cfg(unix)]
    #[test]
    fn a_loose_directory_is_tightened() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = scratch("loose");
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();

        write_state_file(
            &path,
            &DaemonState {
                port: 1,
                token: "t".into(),
            },
        )
        .unwrap();

        let dir_mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "0755 leaks the port and the token");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rewriting_leaves_no_temporary_file_behind() {
        let path = scratch("atomic");
        for port in 1..4u16 {
            write_state_file(
                &path,
                &DaemonState {
                    port,
                    token: "t".into(),
                },
            )
            .unwrap();
        }
        assert_eq!(read_state_file(&path).unwrap().port, 3);
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "state.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "the rename must consume the temp file, found {leftovers:?}"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
