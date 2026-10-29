//! Metadata-only recovery, invoked exclusively while holding owner admission.
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const RECEIPT: &str = "runtime-staging.json";
const MARKER: &str = ".fotw-summary-owner.json";
const LIMIT: u64 = 8192;

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    owner_device: u64,
    owner_inode: u64,
    directory: PathBuf,
    device: u64,
    inode: u64,
    nonce: String,
}

/// Record ownership before copying runtime bytes. An app/process crash before publication
/// can leave only a private directory and tiny ownership metadata, never a
/// large unrecorded package.
pub(super) fn begin(owner: &Path, directory: &Path) -> Result<(), String> {
    let owner_meta = std::fs::symlink_metadata(owner).map_err(|_| recovery_error())?;
    let root_meta = std::fs::symlink_metadata(directory).map_err(|_| recovery_error())?;
    let nonce: [u8; 32] = fotw_secrets::recovery::random_bytes().map_err(|_| recovery_error())?;
    let record = Record {
        version: 1,
        owner_device: owner_meta.dev(),
        owner_inode: owner_meta.ino(),
        directory: directory.into(),
        device: root_meta.dev(),
        inode: root_meta.ino(),
        nonce: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
    };
    validate(&record, owner)?;
    validate_root(&record, &root_meta)?;
    let bytes = serde_json::to_vec(&record).map_err(|_| recovery_error())?;
    let mut marker = options()
        .write(true)
        .create_new(true)
        .open(directory.join(MARKER))
        .map_err(|_| recovery_error())?;
    marker
        .write_all(&bytes)
        .and_then(|_| marker.sync_all())
        .map_err(|_| recovery_error())?;
    sync_directory(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(owner).map_err(|_| recovery_error())?;
    temporary
        .write_all(&bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|_| recovery_error())?;
    temporary
        .persist(owner.join(RECEIPT))
        .map_err(|_| recovery_error())?;
    sync_directory(owner)
}

/// The acquired native-inherited lease proves the prior owner has released
/// admission. Receipt + matching private-directory inode + marker prove which
/// staging belongs to it. Never scan prefixes or signal a recorded PID.
pub(super) fn recover(owner: &Path) -> Result<(), String> {
    let Some(record) = read_record(&owner.join(RECEIPT))? else {
        return Ok(());
    };
    validate(&record, owner)?;
    let root = &record.directory;
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(recovery_error()),
        Ok(metadata) => {
            validate_root(&record, &metadata)?;
            match read_record(&root.join(MARKER))? {
                Some(marker) if marker == record => {
                    // Keep proof through partial deletion failures. Symlinks
                    // are unlinked, including the read-only model aliases.
                    for entry in std::fs::read_dir(root).map_err(|_| recovery_error())? {
                        let entry = entry.map_err(|_| recovery_error())?;
                        if entry.file_name() == MARKER {
                            continue;
                        }
                        let metadata = std::fs::symlink_metadata(entry.path())
                            .map_err(|_| recovery_error())?;
                        if metadata.is_dir() {
                            std::fs::remove_dir_all(entry.path()).map_err(|_| recovery_error())?;
                        } else {
                            std::fs::remove_file(entry.path()).map_err(|_| recovery_error())?;
                        }
                    }
                    // Persist removal of contents before discarding the last
                    // proof. A later empty-root recovery needs only rmdir.
                    sync_directory(root)?;
                    std::fs::remove_file(root.join(MARKER)).map_err(|_| recovery_error())?;
                }
                // A crash between unlinking the last marker and rmdir leaves
                // an empty proven inode. rmdir cannot remove any user content.
                None => {}
                Some(_) => return Err(recovery_error()),
            }
            std::fs::remove_dir(root).map_err(|_| recovery_error())?;
        }
    }
    std::fs::remove_file(owner.join(RECEIPT)).map_err(|_| recovery_error())?;
    sync_directory(owner)
}

fn validate(record: &Record, owner: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(owner).map_err(|_| recovery_error())?;
    let name = record
        .directory
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(recovery_error)?;
    if record.version != 1
        || record.owner_device != metadata.dev()
        || record.owner_inode != metadata.ino()
        || record.directory.parent() != Some(Path::new("/private/tmp"))
        || !name.starts_with("fotw-summary-")
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        || record.nonce.len() != 64
        || !record
            .nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(recovery_error());
    }
    Ok(())
}

fn validate_root(record: &Record, metadata: &std::fs::Metadata) -> Result<(), String> {
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
        || metadata.dev() != record.device
        || metadata.ino() != record.inode
    {
        return Err(recovery_error());
    }
    Ok(())
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options
}

fn read_record(path: &Path) -> Result<Option<Record>, String> {
    let file = match options().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(recovery_error()),
    };
    let metadata = file.metadata().map_err(|_| recovery_error())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > LIMIT
    {
        return Err(recovery_error());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| recovery_error())?;
    if bytes.len() > LIMIT as usize {
        return Err(recovery_error());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| recovery_error())
}

fn sync_directory(directory: &Path) -> Result<(), String> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| recovery_error())
}

fn recovery_error() -> String {
    "local runtime staging recovery unconfirmed; admission refused".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_summary::ownership::OwnerLease;
    use std::os::unix::fs::PermissionsExt;

    fn private_root(prefix: &str, parent: &Path) -> tempfile::TempDir {
        let root = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(parent)
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, OwnerLease) {
        let owner = private_root("owner-", Path::new("/private/tmp"));
        let lease = OwnerLease::acquire(owner.path()).unwrap();
        let root = private_root("fotw-summary-recovery-test-", Path::new("/private/tmp"));
        begin(owner.path(), root.path()).unwrap();
        (owner, root, lease)
    }

    #[test]
    fn active_native_admission_prevents_cleanup_and_recovery_preserves_symlink_targets() {
        let (owner, root, lease) = fixture();
        let unrelated = tempfile::tempdir().unwrap();
        std::fs::write(unrelated.path().join("keep"), "synthetic cached weights").unwrap();
        std::os::unix::fs::symlink(unrelated.path(), root.path().join("model-alias")).unwrap();
        let nested = root.path().join("weights");
        std::fs::create_dir(&nested).unwrap();
        std::os::unix::fs::symlink(unrelated.path(), nested.join("cached-model")).unwrap();
        std::fs::write(root.path().join("llama-server"), "synthetic runtime").unwrap();
        assert!(
            OwnerLease::acquire(owner.path())
                .err()
                .unwrap()
                .contains("busy")
        );
        assert!(root.path().join("llama-server").exists());
        lease.release().unwrap();
        let restarted = OwnerLease::acquire(owner.path()).unwrap();
        assert!(!root.path().exists());
        assert!(!owner.path().join(RECEIPT).exists());
        assert_eq!(
            std::fs::read_to_string(unrelated.path().join("keep")).unwrap(),
            "synthetic cached weights"
        );
        restarted.release().unwrap();
    }

    #[test]
    fn damaged_receipts_or_marker_cannot_authorize_deletion_or_hold_the_lock_forever() {
        let (owner, root, lease) = fixture();
        let receipt = owner.path().join(RECEIPT);
        let marker = root.path().join(MARKER);
        let valid = std::fs::read(&receipt).unwrap();
        std::fs::write(root.path().join("keep"), "synthetic runtime").unwrap();
        lease.release().unwrap();
        for path in [&receipt, &marker] {
            std::fs::write(path, b"corrupt metadata").unwrap();
            assert!(
                OwnerLease::acquire(owner.path())
                    .err()
                    .unwrap()
                    .contains("recovery unconfirmed")
            );
            assert!(root.path().join("keep").exists());
            std::fs::write(path, &valid).unwrap();
        }
        // Correcting metadata after an error must permit a later recovery.
        let restarted = OwnerLease::acquire(owner.path()).unwrap();
        assert!(!root.path().exists());
        restarted.release().unwrap();
    }

    #[test]
    fn transplanted_owner_identity_and_replaced_root_cannot_delete_another_directory() {
        let (owner, root, lease) = fixture();
        let receipt = owner.path().join(RECEIPT);
        let valid = std::fs::read(&receipt).unwrap();
        let mut changed: Record = serde_json::from_slice(&valid).unwrap();
        changed.owner_inode ^= 1;
        std::fs::write(&receipt, serde_json::to_vec(&changed).unwrap()).unwrap();
        lease.release().unwrap();
        assert!(OwnerLease::acquire(owner.path()).is_err());
        assert!(root.path().join(MARKER).exists());
        std::fs::write(&receipt, &valid).unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        std::fs::write(unrelated.path().join("keep"), "unrelated synthetic file").unwrap();
        std::fs::remove_dir_all(root.path()).unwrap();
        std::os::unix::fs::symlink(unrelated.path(), root.path()).unwrap();
        assert!(OwnerLease::acquire(owner.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(unrelated.path().join("keep")).unwrap(),
            "unrelated synthetic file"
        );
        std::fs::remove_file(root.path()).unwrap();
        // A normal TempDir cleanup may already have removed the recorded root.
        OwnerLease::acquire(owner.path())
            .unwrap()
            .release()
            .unwrap();
        assert!(!receipt.exists());
    }

    #[test]
    fn missing_marker_only_allows_removing_an_empty_matching_directory() {
        let (owner, root, lease) = fixture();
        std::fs::remove_file(root.path().join(MARKER)).unwrap();
        std::fs::write(root.path().join("keep"), "synthetic runtime").unwrap();
        lease.release().unwrap();
        assert!(OwnerLease::acquire(owner.path()).is_err());
        assert!(root.path().join("keep").exists());
        std::fs::remove_file(root.path().join("keep")).unwrap();
        OwnerLease::acquire(owner.path())
            .unwrap()
            .release()
            .unwrap();
        assert!(!root.path().exists());
    }
}
