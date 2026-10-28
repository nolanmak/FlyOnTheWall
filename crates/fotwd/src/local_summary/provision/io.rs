use super::super::catalog::DownloadSpec;
use crate::model_setup::Control;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub(super) fn options() -> OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = OpenOptions::new();
    o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o600);
    o
}
pub(super) fn dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
        Ok(_) => return Err("Owned package directory is not an ordinary directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path).map_err(|_| "Cannot create owned package directory")?
        }
        Err(_) => return Err("Cannot inspect owned package directory".into()),
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "Cannot protect owned package directory".into())
}
pub(super) fn regular(path: &Path) -> Result<Option<u64>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(Some(m.len())),
        Ok(_) => Err("Package path is not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Cannot inspect package file".into()),
    }
}
pub(super) fn digest(hash: Sha256) -> String {
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
pub(super) fn verify(
    path: &Path,
    expected: u64,
    sha: &str,
    control: &Control,
) -> Result<bool, String> {
    if regular(path)? != Some(expected) {
        return Ok(false);
    }
    let mut file = options()
        .read(true)
        .open(path)
        .map_err(|_| "Cannot read package file")?;
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    let mut buf = [0; 65536];
    loop {
        control.check()?;
        let n = file
            .read(&mut buf)
            .map_err(|_| "Package verification read failed")?;
        if n == 0 {
            break;
        }
        count = count
            .checked_add(n as u64)
            .ok_or("Package length overflow")?;
        if count > expected {
            return Ok(false);
        }
        hash.update(&buf[..n]);
    }
    control.check()?;
    Ok(count == expected && digest(hash) == sha)
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u32,
    artifact_sha256: String,
    bytes: u64,
    prefix_sha256: String,
}
pub(super) struct Retained {
    pub file: File,
    pub bytes: u64,
    pub hash: Sha256,
}
pub(super) fn restore(
    path: &Path,
    checkpoint: &Path,
    spec: &DownloadSpec,
    control: &Control,
) -> Result<Retained, String> {
    regular(path)?;
    let mut file = options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| "Cannot open owned transfer file")?;
    let record = match regular(checkpoint)? {
        Some(n) if n <= 4096 => {
            let mut f = options()
                .read(true)
                .open(checkpoint)
                .map_err(|_| "Cannot open transfer checkpoint")?;
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)
                .map_err(|_| "Cannot read transfer checkpoint")?;
            serde_json::from_slice::<Checkpoint>(&bytes).ok()
        }
        _ => None,
    };
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    if let Some(c) = record
        .filter(|c| c.version == 1 && c.artifact_sha256 == spec.sha256() && c.bytes <= spec.bytes())
    {
        let mut buf = [0; 65536];
        while count < c.bytes {
            control.check()?;
            let max = (c.bytes - count).min(buf.len() as u64) as usize;
            let n = file
                .read(&mut buf[..max])
                .map_err(|_| "Retained transfer read failed")?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            count += n as u64;
        }
        if count != c.bytes || digest(hash.clone()) != c.prefix_sha256 {
            count = 0;
            hash = Sha256::new();
        }
    }
    file.set_len(count)
        .map_err(|_| "Cannot trim unverified transfer tail")?;
    file.seek(SeekFrom::Start(count))
        .map_err(|_| "Cannot resume transfer file")?;
    Ok(Retained {
        file,
        bytes: count,
        hash,
    })
}
pub(super) fn checkpoint(
    path: &Path,
    spec: &DownloadSpec,
    state: &mut Retained,
) -> Result<(), String> {
    state
        .file
        .flush()
        .and_then(|_| state.file.sync_data())
        .map_err(|_| "Cannot checkpoint transferred bytes")?;
    let data = Checkpoint {
        version: 1,
        artifact_sha256: spec.sha256().into(),
        bytes: state.bytes,
        prefix_sha256: digest(state.hash.clone()),
    };
    write_json(path, &data)
}
pub(super) fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let temp = path.with_extension("pending");
    if regular(&temp)?.is_some() {
        std::fs::remove_file(&temp).map_err(|_| "Cannot clear interrupted metadata write")?;
    }
    regular(path)?;
    let bytes = serde_json::to_vec(value).map_err(|_| "Cannot encode package metadata")?;
    let mut file = options()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|_| "Cannot create package metadata")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Cannot persist package metadata")?;
    drop(file);
    std::fs::rename(&temp, path).map_err(|_| "Cannot publish package metadata")?;
    sync_dir(
        path.parent()
            .ok_or("Package metadata has no parent directory")?,
    )
}
pub(super) fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|_| "Cannot confirm package directory durability".into())
}
