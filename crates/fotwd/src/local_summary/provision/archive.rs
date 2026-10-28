use super::{super::catalog::CatalogRuntime, io};
use crate::model_setup::Control;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Write},
    path::Path,
};

pub(super) fn prepare(
    stage: &Path,
    runtime: &CatalogRuntime,
    control: &Control,
) -> Result<(), String> {
    let unpack = stage.join("runtime-source");
    let output = stage.join("runtime");
    for p in [&unpack, &output] {
        if p.exists() {
            if std::fs::symlink_metadata(p)
                .map_err(|_| "Cannot inspect runtime stage")?
                .file_type()
                .is_symlink()
            {
                return Err("Runtime stage is a symlink".into());
            }
            std::fs::remove_dir_all(p).map_err(|_| "Cannot reset interrupted runtime stage")?;
        }
        io::dir(p)?;
    }
    let required: BTreeMap<_, _> = runtime
        .files
        .iter()
        .chain(&runtime.notices)
        .map(|f| (f.source_path.as_str(), f))
        .collect();
    let archive = io::options()
        .read(true)
        .open(stage.join(&runtime.archive_name))
        .map_err(|_| "Runtime archive unavailable")?;
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut archive = tar::Archive::new(decoder);
    let prefix = format!("llama-{}/", runtime.tag);
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    for (index, entry) in archive
        .entries()
        .map_err(|_| "Invalid runtime archive")?
        .enumerate()
    {
        control.check()?;
        if index >= 2048 {
            return Err("Runtime archive entry limit exceeded".into());
        }
        let mut entry = entry.map_err(|_| "Invalid runtime archive entry")?;
        let path = std::str::from_utf8(&entry.path_bytes())
            .map_err(|_| "Runtime archive path is not UTF-8")?
            .to_owned();
        if (path == format!("llama-{}", runtime.tag) || path == prefix)
            && entry.header().entry_type().is_dir()
        {
            continue;
        }
        let name = path
            .strip_prefix(&prefix)
            .ok_or("Runtime archive escapes its release directory")?;
        if name.is_empty()
            || name.contains('/')
            || name == "."
            || name == ".."
            || name.contains('\\')
            || !seen.insert(name.to_ascii_lowercase())
        {
            return Err("Unsafe or duplicate runtime archive path".into());
        }
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or("Runtime archive size overflow")?;
        if total > runtime.unpack_extra_bytes {
            return Err("Runtime archive exceeds declared unpack bound".into());
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() {
            let target = entry
                .link_name()
                .map_err(|_| "Invalid runtime archive alias")?
                .ok_or("Missing runtime alias")?;
            let target = target.to_str().ok_or("Invalid runtime alias")?;
            if target.is_empty()
                || target.contains('/')
                || target.contains('\\')
                || target == "."
                || target == ".."
            {
                return Err("Runtime archive alias escapes its directory".into());
            }
            continue; // Never materialize archive links. Flatten only catalog-pinned sources.
        }
        if !kind.is_file() {
            return Err("Unsupported runtime archive entry type".into());
        }
        let Some(expected) = required.get(name) else {
            let mut buf = [0; 65536];
            loop {
                control.check()?;
                if entry
                    .read(&mut buf)
                    .map_err(|_| "Runtime archive read failed")?
                    == 0
                {
                    break;
                }
            }
            continue;
        };
        if size != expected.bytes {
            return Err("Runtime archive file size mismatch".into());
        }
        let mut file = io::options()
            .write(true)
            .create_new(true)
            .open(unpack.join(name))
            .map_err(|_| "Cannot stage runtime file")?;
        let mut hash = Sha256::new();
        let mut copied = 0_u64;
        let mut buf = [0; 65536];
        loop {
            control.check()?;
            let n = entry
                .read(&mut buf)
                .map_err(|_| "Runtime archive read failed")?;
            if n == 0 {
                break;
            }
            copied = copied
                .checked_add(n as u64)
                .ok_or("Runtime file size overflow")?;
            if copied > expected.bytes {
                return Err("Runtime archive file exceeds its bound".into());
            }
            file.write_all(&buf[..n])
                .map_err(|_| "Runtime staging write failed")?;
            hash.update(&buf[..n]);
        }
        if copied != expected.bytes || io::digest(hash) != expected.sha256 {
            return Err("Runtime archive file checksum mismatch".into());
        }
        file.sync_all().map_err(|_| "Cannot persist runtime file")?;
    }
    use std::os::unix::fs::PermissionsExt;
    for expected in runtime.files.iter().chain(&runtime.notices) {
        control.check()?;
        let source = unpack.join(&expected.source_path);
        if !io::verify(&source, expected.bytes, &expected.sha256, control)? {
            return Err("Runtime archive omitted a pinned file".into());
        }
        let mut input = io::options()
            .read(true)
            .open(source)
            .map_err(|_| "Pinned runtime source unavailable")?;
        let target = output.join(&expected.name);
        let mut out = io::options()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|_| "Cannot flatten runtime file")?;
        let mut buf = [0; 65536];
        loop {
            control.check()?;
            let n = input
                .read(&mut buf)
                .map_err(|_| "Runtime source read failed")?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .map_err(|_| "Runtime staging write failed")?;
        }
        out.sync_all().map_err(|_| "Cannot persist runtime file")?;
        if expected.name == "llama-server" {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "Cannot set runtime executable permissions")?;
        }
    }
    io::sync_dir(&output)?;
    std::fs::remove_dir_all(unpack).map_err(|_| "Cannot retire runtime extraction stage")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::catalog::{CatalogFile, CatalogRuntime};
    use super::*;
    fn sha(b: &[u8]) -> String {
        io::digest(Sha256::new().chain_update(b))
    }
    fn runtime() -> CatalogRuntime {
        let file = |name: &str, source: &str, b: &[u8]| CatalogFile {
            name: name.into(),
            source_path: source.into(),
            bytes: b.len() as u64,
            sha256: sha(b),
        };
        CatalogRuntime {
            protocol: "llama_cpp_http_v1".into(),
            source_revision: "a".repeat(40),
            tag: "b1".into(),
            archive_name: "runtime.tar.gz".into(),
            archive_bytes: 1,
            archive_sha256: "b".repeat(64),
            unpack_extra_bytes: 65536,
            license: "MIT".into(),
            files: vec![
                file("llama-server", "llama-server", b"exe"),
                file("lib.dylib", "lib.1.dylib", b"lib"),
            ],
            notices: vec![file("LICENSE", "LICENSE", b"license")],
        }
    }
    fn archive(root: &Path, entries: &[(&str, tar::EntryType, &str, &[u8])]) {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, kind, link, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o600);
            header.set_size(bytes.len() as u64);
            // Raw path bytes deliberately allow adversarial fixtures that a
            // normal archive builder's safe path API refuses to construct.
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            if !link.is_empty() {
                header.set_link_name(link).unwrap();
            }
            header.set_cksum();
            tar.append(&header, *bytes).unwrap();
        }
        let data = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&data).unwrap();
        std::fs::write(root.join("runtime.tar.gz"), gz.finish().unwrap()).unwrap();
    }
    #[test]
    fn flattens_only_pinned_sources_into_regular_files_and_retains_license() {
        let root = tempfile::tempdir().unwrap();
        archive(
            root.path(),
            &[
                ("llama-b1/llama-server", tar::EntryType::Regular, "", b"exe"),
                ("llama-b1/lib.1.dylib", tar::EntryType::Regular, "", b"lib"),
                (
                    "llama-b1/lib.dylib",
                    tar::EntryType::Symlink,
                    "lib.1.dylib",
                    b"",
                ),
                ("llama-b1/LICENSE", tar::EntryType::Regular, "", b"license"),
            ],
        );
        prepare(root.path(), &runtime(), &Control::default()).unwrap();
        assert!(
            !std::fs::symlink_metadata(root.path().join("runtime/lib.dylib"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(root.path().join("runtime/lib.dylib")).unwrap(),
            b"lib"
        );
        assert_eq!(
            std::fs::read(root.path().join("runtime/LICENSE")).unwrap(),
            b"license"
        );
    }
    #[test]
    fn refuses_escape_duplicate_links_devices_and_oversized_unpacked_content() {
        for (name, kind, link, bytes) in [
            ("../escape", tar::EntryType::Regular, "", b"bad".as_slice()),
            (
                "llama-b1/../escape",
                tar::EntryType::Regular,
                "",
                b"bad".as_slice(),
            ),
            (
                "llama-b1/alias",
                tar::EntryType::Symlink,
                "../escape",
                b"".as_slice(),
            ),
            (
                "llama-b1/hardlink",
                tar::EntryType::Link,
                "llama-server",
                b"".as_slice(),
            ),
            ("llama-b1/device", tar::EntryType::Char, "", b"".as_slice()),
            (
                "llama-b1/llama-server",
                tar::EntryType::Regular,
                "",
                b"wrong".as_slice(),
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            archive(root.path(), &[(name, kind, link, bytes)]);
            assert!(
                prepare(root.path(), &runtime(), &Control::default()).is_err(),
                "{name}"
            );
            assert!(!root.path().join("escape").exists());
        }
        let root = tempfile::tempdir().unwrap();
        archive(
            root.path(),
            &[
                ("llama-b1/extra", tar::EntryType::Regular, "", b"x"),
                ("llama-b1/EXTRA", tar::EntryType::Regular, "", b"x"),
            ],
        );
        assert!(
            prepare(root.path(), &runtime(), &Control::default())
                .unwrap_err()
                .contains("duplicate")
        );
        let root = tempfile::tempdir().unwrap();
        archive(
            root.path(),
            &[(
                "llama-b1/extra",
                tar::EntryType::Regular,
                "",
                &vec![0; 65537],
            )],
        );
        assert!(
            prepare(root.path(), &runtime(), &Control::default())
                .unwrap_err()
                .contains("unpack bound")
        );
    }
}
