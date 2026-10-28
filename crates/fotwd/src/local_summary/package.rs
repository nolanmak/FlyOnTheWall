use super::process::{Artifact, LoadCancellation, verified_copy_cancellable};
use std::{
    fs::File,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

pub(super) struct BoundModel {
    pub(super) files: Vec<File>,
    pub(super) first_path: PathBuf,
    _directory: tempfile::TempDir,
}

#[cfg(test)]
pub(super) fn bind_models(
    first: &Artifact,
    shards: &[Artifact],
    root: &Path,
) -> Result<BoundModel, String> {
    bind_models_with_cancellation(first, shards, root, &LoadCancellation::default())
}

pub(super) fn bind_models_with_cancellation(
    first: &Artifact,
    shards: &[Artifact],
    root: &Path,
    cancellation: &LoadCancellation,
) -> Result<BoundModel, String> {
    cancellation.check()?;
    let directory = tempfile::Builder::new()
        .prefix("weights-")
        .tempdir_in(root)
        .map_err(|_| "private model directory unavailable")?;
    let mut names = std::collections::HashSet::new();
    let mut files = Vec::new();
    let mut first_path = None;
    for artifact in std::iter::once(first).chain(shards) {
        let name = artifact
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("invalid model filename")?;
        if !name.ends_with(".gguf")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !names.insert(name.to_ascii_lowercase())
        {
            return Err("ambiguous or invalid model filename".into());
        }
        let file = verified_copy_cancellable(artifact, None, None, cancellation)?;
        let alias = directory.path().join(name);
        // Keep the filename pattern that llama.cpp uses to discover the next
        // GGUF split, while binding every alias to an already verified inode.
        // These private aliases are created by us, never accepted as inputs.
        std::os::unix::fs::symlink(format!("/dev/fd/{}", file.as_raw_fd()), &alias)
            .map_err(|_| "private model binding failed")?;
        if first_path.is_none() {
            first_path = Some(alias);
        }
        files.push(file);
    }
    Ok(BoundModel {
        files,
        first_path: first_path.ok_or("model package is empty")?,
        _directory: directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    fn artifact(root: &Path, name: &str, bytes: &[u8]) -> Artifact {
        let path = root.join(name);
        std::fs::write(&path, bytes).unwrap();
        Artifact {
            path,
            sha256: Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        }
    }
    #[test]
    fn all_shards_keep_verified_inodes_and_native_discovery_filenames() {
        let cache = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let first = artifact(
            cache.path(),
            "model-00001-of-00002.gguf",
            b"synthetic first",
        );
        let second = artifact(
            cache.path(),
            "model-00002-of-00002.gguf",
            b"synthetic second",
        );
        let bound = bind_models(&first, std::slice::from_ref(&second), private.path()).unwrap();
        assert_eq!(bound.files.len(), 2);
        assert_eq!(
            bound.first_path.file_name().unwrap(),
            "model-00001-of-00002.gguf"
        );
        for item in [&first, &second] {
            std::fs::rename(&item.path, item.path.with_extension("old")).unwrap();
            std::fs::write(&item.path, b"replacement must not load").unwrap();
        }
        assert_eq!(
            std::fs::read(&bound.first_path).unwrap(),
            b"synthetic first"
        );
        assert_eq!(
            std::fs::read(bound.first_path.with_file_name("model-00002-of-00002.gguf")).unwrap(),
            b"synthetic second"
        );
    }
    #[test]
    fn corrupt_or_missing_later_shard_cannot_become_a_bound_package() {
        let cache = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let first = artifact(cache.path(), "model-00001-of-00002.gguf", b"first");
        let mut second = artifact(cache.path(), "model-00002-of-00002.gguf", b"second");
        second.sha256 = "0".repeat(64);
        assert!(
            bind_models(&first, std::slice::from_ref(&second), private.path())
                .err()
                .unwrap()
                .contains("checksum")
        );
        std::fs::remove_file(&second.path).unwrap();
        assert!(
            bind_models(&first, &[second], private.path())
                .err()
                .unwrap()
                .contains("unavailable")
        );
    }
    #[test]
    fn ambiguous_filenames_cannot_replace_another_shards_alias() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let first = artifact(a.path(), "model.gguf", b"first");
        let second = artifact(b.path(), "MODEL.gguf", b"second");
        assert!(
            bind_models(&first, &[second], private.path())
                .err()
                .unwrap()
                .contains("filename")
        );
    }
}
