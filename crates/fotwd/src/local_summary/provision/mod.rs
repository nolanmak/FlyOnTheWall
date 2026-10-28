//! Explicit provisioning runs on an owned blocking worker. Model inference,
//! status reads and catalog lookup do not invoke this module automatically.
use super::catalog::{Catalog, DownloadSpec};
use crate::model_setup::Control;
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// Content-free installation progress. Installed files are not a loaded or
/// quality-qualified model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Inspect retained bytes.
    Checking,
    /// Transfer public artifacts.
    Downloading,
    /// Validate complete identities.
    Verifying,
    /// Prepare pinned runtime files.
    Unpacking,
    /// All bytes and receipts are verified; publication has not begun yet.
    Publishing,
    /// Atomic publication finished; native load is still separate.
    Installed,
}
/// Authoritative byte counts across all package downloads.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    /// Current phase.
    pub phase: Phase,
    /// Actual retained/transferred bytes.
    pub completed: u64,
    /// Exact pinned total.
    pub total: u64,
}
/// Source response, injected for owned fixtures. Production uses restricted HTTPS.
pub(super) struct Response {
    pub status: u16,
    pub length: Option<u64>,
    pub range: Option<(u64, u64, u64)>,
    pub body: Box<dyn Read + Send>,
}
pub(super) trait Transport {
    fn open(&mut self, spec: &DownloadSpec, offset: u64) -> Result<Response, String>;
}

mod archive;
mod http;
mod io;

/// An explicit provisioning worker. Cancellation acknowledgement is separate
/// from completion; retain this job and await `wait` before replacing its owner.
pub struct Job {
    control: Control,
    task: tokio::task::JoinHandle<Result<PathBuf, String>>,
}
impl Job {
    /// Request cancellation; false means publication has already started.
    pub fn cancel(&self) -> bool {
        self.control.cancel()
    }
    /// Await actual I/O/worker completion. Installed artifacts still require
    /// qualified configuration/admission and a native load before Ready.
    ///
    /// # Errors
    /// Integrity, source, disk, cancellation or owned-worker failure.
    pub async fn wait(self) -> Result<PathBuf, String> {
        self.task
            .await
            .map_err(|_| "Artifact provisioning worker failed")?
    }
}
/// Begin only after an explicit Download action, on a Tokio runtime. Neither
/// recording, summary retries nor catalog refresh should call this function.
///
/// # Errors
/// Refuses a missing runtime executor; setup failures are returned by `wait`.
pub fn begin(
    catalog: std::sync::Arc<Catalog>,
    model_id: String,
    parent: PathBuf,
    mut progress: impl FnMut(Progress) + Send + 'static,
) -> Result<Job, String> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| "Provisioning requires an owned runtime")?;
    let control = Control::default();
    let worker_control = control.clone();
    let task = runtime.spawn_blocking(move || {
        let mut transport = http::Https::new()?;
        install_with(
            &catalog,
            &model_id,
            &parent,
            &worker_control,
            &mut transport,
            &|p| {
                super::probe::disk_available(p)
                    .ok_or_else(|| "Current disk space is unavailable".into())
            },
            &mut progress,
        )
    });
    Ok(Job { control, task })
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    bundle: String,
    model_sha256: String,
    runtime_sha256: String,
    catalog_revision: String,
}
fn receipt_read(path: &Path) -> Result<Option<Receipt>, String> {
    let Some(n) = io::regular(path)? else {
        return Ok(None);
    };
    if n > 8192 {
        return Ok(None);
    }
    let mut file = io::options()
        .read(true)
        .open(path)
        .map_err(|_| "Cannot open package receipt")?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|_| "Cannot read package receipt")?;
    Ok(serde_json::from_slice(&data).ok())
}
fn installed(
    path: &Path,
    catalog: &Catalog,
    specs: &[DownloadSpec],
    expected: &Receipt,
    control: &Control,
) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let meta = std::fs::symlink_metadata(path).map_err(|_| "Cannot inspect installed package")?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("Installed package path is not an owned directory".into());
    }
    let Some(receipt) = receipt_read(&path.join("installed.json"))? else {
        return Ok(false);
    };
    if receipt.version != 1
        || receipt.bundle != expected.bundle
        || receipt.model_sha256 != expected.model_sha256
        || receipt.runtime_sha256 != expected.runtime_sha256
    {
        return Ok(false);
    }
    for spec in specs {
        if !io::verify(
            &path.join(spec.name()),
            spec.bytes(),
            spec.sha256(),
            control,
        )? {
            return Ok(false);
        }
    }
    for file in catalog
        .runtime()
        .files
        .iter()
        .chain(&catalog.runtime().notices)
    {
        if !io::verify(
            &path.join("runtime").join(&file.name),
            file.bytes,
            &file.sha256,
            control,
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}
// Explicit release also covers a descriptor inherited by a concurrent fork
// that has not reached exec/CLOEXEC yet. Only this worker owns this lease.
struct PackageLease(std::fs::File);
impl Drop for PackageLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
fn install_with(
    catalog: &Catalog,
    model_id: &str,
    parent: &Path,
    control: &Control,
    transport: &mut dyn Transport,
    available: &dyn Fn(&Path) -> Result<u64, String>,
    progress: &mut dyn FnMut(Progress),
) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom, Write};
    control.check()?;
    let model = catalog
        .model(model_id)
        .ok_or("Model is not in the pinned catalog")?;
    let mut specs = vec![catalog.runtime_download()];
    specs.extend(
        catalog
            .model_downloads(model_id)
            .ok_or("Model is not in the pinned catalog")?,
    );
    let total = specs
        .iter()
        .try_fold(0_u64, |n, s| n.checked_add(s.bytes()))
        .ok_or("Package size overflow")?;
    let model_sha = model.package_sha256();
    let bundle = io::digest(
        Sha256::new()
            .chain_update(model_sha.as_bytes())
            .chain_update(catalog.runtime().archive_sha256.as_bytes()),
    );
    let receipt = Receipt {
        version: 1,
        bundle: bundle.clone(),
        model_sha256: model_sha,
        runtime_sha256: catalog.runtime().archive_sha256.clone(),
        catalog_revision: catalog.revision().into(),
    };
    let root = parent.join("local-summary-models");
    io::dir(&root)?;
    let lock = io::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(format!("{bundle}.lock")))
        .map_err(|_| "Cannot open package ownership lock")?;
    if !lock
        .metadata()
        .map_err(|_| "Cannot inspect package ownership lock")?
        .is_file()
    {
        return Err("Package ownership lock is not regular".into());
    }
    lock.try_lock()
        .map_err(|_| "Another setup or inference owner still holds this package")?;
    let _lease = PackageLease(lock);
    let target = root.join(&bundle);
    let backup = root.join(format!(".{bundle}.previous"));
    if let Ok(meta) = std::fs::symlink_metadata(&backup) {
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || !receipt_read(&backup.join("installed.json"))?
                .is_some_and(|r| r.version == 1 && r.bundle == bundle)
        {
            return Err("Retained package identity is not recognized".into());
        }
        if !target
            .try_exists()
            .map_err(|_| "Cannot inspect installed package")?
        {
            // Crash between retaining the old directory and publishing the new.
            // Restore first; content verification below decides whether to reuse.
            std::fs::rename(&backup, &target).map_err(|_| "Cannot recover retained package")?;
            io::sync_dir(&root)?;
        }
    }
    progress(Progress {
        phase: Phase::Checking,
        completed: 0,
        total,
    });
    if installed(&target, catalog, &specs, &receipt, control)? {
        control.commit()?;
        if backup.exists() {
            std::fs::remove_dir_all(&backup)
                .map_err(|_| "Cannot retire verified replacement backup")?;
            io::sync_dir(&root)?;
        }
        progress(Progress {
            phase: Phase::Installed,
            completed: total,
            total,
        });
        return Ok(target);
    }
    let stage = root.join(format!(".{bundle}.partial"));
    io::dir(&stage)?;
    let marker = stage.join("plan.json");
    match receipt_read(&marker)? {
        Some(r) if r.bundle == bundle && r.version == 1 => {}
        Some(_) => return Err("Staging package identity mismatch".into()),
        None => {
            if std::fs::read_dir(&stage)
                .map_err(|_| "Cannot inspect package stage")?
                .next()
                .is_some()
            {
                return Err("Unrecognized nonempty package stage".into());
            }
            io::write_json(&marker, &receipt)?;
        }
    }
    let mut states = Vec::new();
    let mut completed = 0_u64;
    for spec in &specs {
        control.check()?;
        let state = io::restore(
            &stage.join(spec.name()),
            &stage.join(format!("{}.resume.json", spec.sha256())),
            spec,
            control,
        )?;
        completed = completed
            .checked_add(state.bytes)
            .ok_or("Package progress overflow")?;
        states.push(state);
    }
    let extra = catalog.runtime().unpack_extra_bytes;
    let check_disk = |remaining: u64| -> Result<(), String> {
        let required = remaining
            .checked_add(extra)
            .and_then(|n| n.checked_add(2_u64 << 30))
            .ok_or("Package disk requirement overflow")?;
        if available(&root)? < required {
            return Err(
                "Not enough free space for remaining download, unpack and 2 GiB reserve".into(),
            );
        }
        Ok(())
    };
    check_disk(total - completed)?;
    for (spec, state) in specs.iter().zip(&mut states) {
        let checkpoint = stage.join(format!("{}.resume.json", spec.sha256()));
        if state.bytes == spec.bytes() && io::digest(state.hash.clone()) == spec.sha256() {
            continue;
        }
        if state.bytes == spec.bytes() {
            completed -= state.bytes;
            state.bytes = 0;
            state.hash = Sha256::new();
            state
                .file
                .set_len(0)
                .and_then(|_| state.file.seek(SeekFrom::Start(0)))
                .map_err(|_| "Cannot reset corrupt transfer")?;
        }
        control.check()?;
        check_disk(total - completed)?;
        let mut response = transport.open(spec, state.bytes)?;
        if response.status == 200 {
            if response.length.is_some_and(|n| n != spec.bytes()) {
                return Err("Artifact server length mismatch".into());
            }
            if state.bytes > 0 {
                completed -= state.bytes;
                state.bytes = 0;
                state.hash = Sha256::new();
                state
                    .file
                    .set_len(0)
                    .and_then(|_| state.file.seek(SeekFrom::Start(0)))
                    .map_err(|_| "Cannot restart unresumable transfer")?;
                check_disk(total - completed)?;
            }
        } else if response.status == 206 {
            if response.range != Some((state.bytes, spec.bytes() - 1, spec.bytes()))
                || response
                    .length
                    .is_some_and(|n| n != spec.bytes() - state.bytes)
            {
                return Err("Artifact resume range mismatch".into());
            }
        } else {
            return Err("Artifact server refused the pinned source".into());
        }
        let mut checkpoint_bytes = state.bytes;
        let mut checkpoint_time = std::time::Instant::now();
        let mut buffer = [0_u8; 65536];
        loop {
            if let Err(e) = control.check() {
                io::checkpoint(&checkpoint, spec, state)?;
                return Err(e);
            }
            let n = match response.body.read(&mut buffer) {
                Ok(n) => n,
                Err(_) => {
                    io::checkpoint(&checkpoint, spec, state)?;
                    return Err(
                        "Artifact transfer interrupted; verified retained bytes can be resumed"
                            .into(),
                    );
                }
            };
            if let Err(e) = control.check() {
                io::checkpoint(&checkpoint, spec, state)?;
                return Err(e);
            }
            if n == 0 {
                break;
            }
            let next = state
                .bytes
                .checked_add(n as u64)
                .ok_or("Artifact progress overflow")?;
            if next > spec.bytes() {
                return Err("Artifact exceeds its pinned size".into());
            }
            state
                .file
                .write_all(&buffer[..n])
                .map_err(|_| "Artifact write failed; previous installation preserved")?;
            state.hash.update(&buffer[..n]);
            state.bytes = next;
            completed = completed
                .checked_add(n as u64)
                .ok_or("Package progress overflow")?;
            if state.bytes - checkpoint_bytes >= 8 << 20
                || checkpoint_time.elapsed() >= std::time::Duration::from_secs(2)
            {
                io::checkpoint(&checkpoint, spec, state)?;
                check_disk(total - completed)?;
                checkpoint_bytes = state.bytes;
                checkpoint_time = std::time::Instant::now();
            }
            progress(Progress {
                phase: Phase::Downloading,
                completed,
                total,
            });
        }
        io::checkpoint(&checkpoint, spec, state)?;
        progress(Progress {
            phase: Phase::Verifying,
            completed,
            total,
        });
        control.check()?;
        if state.bytes != spec.bytes() || io::digest(state.hash.clone()) != spec.sha256() {
            return Err(
                "Artifact size or checksum mismatch; previous installation preserved".into(),
            );
        }
    }
    drop(states);
    control.check()?;
    check_disk(0)?;
    progress(Progress {
        phase: Phase::Unpacking,
        completed,
        total,
    });
    control.check()?;
    archive::prepare(&stage, catalog.runtime(), control)?;
    control.check()?;
    io::write_json(&stage.join("installed.json"), &receipt)?;
    io::sync_dir(&stage)?;
    progress(Progress {
        phase: Phase::Publishing,
        completed,
        total,
    });
    control.commit()?;
    if target.exists() {
        if backup.exists() {
            return Err("Previous interrupted replacement needs recovery".into());
        }
        std::fs::rename(&target, &backup).map_err(|_| "Cannot retain previous installation")?;
    }
    if std::fs::rename(&stage, &target).is_err() {
        if backup.exists() {
            let _ = std::fs::rename(&backup, &target);
        }
        return Err("Cannot publish verified package; previous installation preserved".into());
    }
    io::sync_dir(&root)?;
    if backup.exists() {
        std::fs::remove_dir_all(&backup)
            .map_err(|_| "Verified package installed but previous cache retirement failed")?;
    }
    progress(Progress {
        phase: Phase::Installed,
        completed: total,
        total,
    });
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    struct Fixture {
        catalog: Catalog,
        archive: Vec<u8>,
        weights: Vec<u8>,
    }
    fn fixture() -> Fixture {
        fixture_with_weights(b"synthetic weights with several chunks".to_vec())
    }
    fn fixture_with_weights(weights: Vec<u8>) -> Fixture {
        let executable = b"synthetic executable";
        let notice = b"synthetic license";
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in [
            ("llama-server", executable.as_slice()),
            ("LICENSE", notice.as_slice()),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o600);
            h.set_cksum();
            builder
                .append_data(&mut h, format!("llama-b1/{name}"), bytes)
                .unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut encoder, &tar).unwrap();
        let archive = encoder.finish().unwrap();
        let file = |name: &str, bytes: &[u8]| json!({"name":name,"source_path":name,"bytes":bytes.len(),"sha256":sha(bytes)});
        let data = json!({"format_version":1,"revision":"fixture-1","runtime":{"protocol":"llama_cpp_http_v1","source_revision":"a".repeat(40),"tag":"b1","archive_name":"llama-b1-bin-macos-arm64.tar.gz","archive_bytes":archive.len(),"archive_sha256":sha(&archive),"unpack_extra_bytes":65536,"license":"MIT","files":[file("llama-server",executable)],"notices":[file("LICENSE",notice)]},"models":[{"id":"fixture","label":"Fixture","quantization":"Q8_0","repository":"Fixture/Model","revision":"b".repeat(40),"license":"Apache-2.0","files":[file("model.gguf",&weights)]}]});
        Fixture {
            catalog: super::super::catalog::test_catalog(&data.to_string()).unwrap(),
            archive,
            weights,
        }
    }
    struct Source {
        archive: Vec<u8>,
        weights: Vec<u8>,
        opens: Vec<(String, u64)>,
        fail_after: Option<usize>,
    }
    struct Stops {
        bytes: std::io::Cursor<Vec<u8>>,
        remaining: usize,
    }
    impl Read for Stops {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("fixture disconnect"));
            }
            let max = out.len().min(self.remaining);
            let n = self.bytes.read(&mut out[..max])?;
            self.remaining -= n;
            Ok(n)
        }
    }
    impl Transport for Source {
        fn open(&mut self, spec: &DownloadSpec, offset: u64) -> Result<Response, String> {
            self.opens.push((spec.name().into(), offset));
            let full = if spec.name().ends_with(".gz") {
                &self.archive
            } else {
                &self.weights
            };
            let data = full
                .get(offset as usize..)
                .ok_or("invalid offset")?
                .to_vec();
            let body: Box<dyn Read + Send> = match self.fail_after.take() {
                Some(n) => Box::new(Stops {
                    bytes: std::io::Cursor::new(data),
                    remaining: n,
                }),
                None => Box::new(std::io::Cursor::new(data)),
            };
            Ok(Response {
                status: if offset == 0 { 200 } else { 206 },
                length: Some(full.len() as u64 - offset),
                range: if offset == 0 {
                    None
                } else {
                    Some((offset, full.len() as u64 - 1, full.len() as u64))
                },
                body,
            })
        }
    }
    fn source(f: &Fixture) -> Source {
        Source {
            archive: f.archive.clone(),
            weights: f.weights.clone(),
            opens: vec![],
            fail_after: None,
        }
    }
    fn install(
        f: &Fixture,
        root: &Path,
        control: &Control,
        source: &mut Source,
        available: u64,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<PathBuf, String> {
        install_with(
            &f.catalog,
            "fixture",
            root,
            control,
            source,
            &|_| Ok(available),
            progress,
        )
    }
    #[test]
    fn verifies_extracts_and_restarts_offline_without_opening_sources() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut s = source(&f);
        let mut phases = vec![];
        let installed = install(
            &f,
            root.path(),
            &Control::default(),
            &mut s,
            u64::MAX,
            &mut |p| phases.push(p.phase),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(installed.join("runtime/llama-server")).unwrap(),
            b"synthetic executable"
        );
        assert_eq!(
            std::fs::read(installed.join("model.gguf")).unwrap(),
            f.weights
        );
        assert!(phases.contains(&Phase::Unpacking));
        assert_eq!(phases.last(), Some(&Phase::Installed));
        s.opens.clear();
        assert_eq!(
            install(
                &f,
                root.path(),
                &Control::default(),
                &mut s,
                u64::MAX,
                &mut |_| {}
            )
            .unwrap(),
            installed
        );
        assert!(s.opens.is_empty());
    }
    #[test]
    fn interrupted_transfer_resumes_only_verified_retained_bytes() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut s = source(&f);
        s.fail_after = Some(17);
        assert!(
            install(
                &f,
                root.path(),
                &Control::default(),
                &mut s,
                u64::MAX,
                &mut |_| {}
            )
            .is_err()
        );
        s.opens.clear();
        install(
            &f,
            root.path(),
            &Control::default(),
            &mut s,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            s.opens[0].1, 17,
            "resume the checkpointed prefix rather than restart all bytes"
        );
    }
    #[test]
    fn low_disk_refuses_before_source_access_and_preserves_other_installations() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("previous-working"), b"keep").unwrap();
        let mut s = source(&f);
        let e = install(
            &f,
            root.path(),
            &Control::default(),
            &mut s,
            2_u64 << 30,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(e.contains("space"), "{e}");
        assert!(s.opens.is_empty());
        assert_eq!(
            std::fs::read(root.path().join("previous-working")).unwrap(),
            b"keep"
        );
    }
    #[test]
    fn cancel_before_publication_retains_previous_state_and_never_reports_installed() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut s = source(&f);
        let c = Control::default();
        let mut installed = false;
        let e = install(&f, root.path(), &c, &mut s, u64::MAX, &mut |p| {
            if p.phase == Phase::Verifying {
                c.cancel();
            }
            if p.phase == Phase::Installed {
                installed = true;
            }
        })
        .unwrap_err();
        assert!(e.contains("cancelled"), "{e}");
        assert!(!installed);
    }
    #[test]
    fn final_cancel_wins_after_unpack_before_the_publication_commit() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut s = source(&f);
        let c = Control::default();
        let mut reached = false;
        let result = install(&f, root.path(), &c, &mut s, u64::MAX, &mut |p| {
            if p.phase == Phase::Publishing {
                reached = true;
                assert!(c.cancel());
            }
        });
        assert!(reached, "must expose the actual pre-publication boundary");
        assert!(result.unwrap_err().contains("cancelled"));
        assert!(
            !std::fs::read_dir(root.path().join("local-summary-models"))
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.path().join("installed.json").exists()
                    && !e.file_name().to_string_lossy().starts_with('.'))
        );
    }

    fn staged(root: &Path) -> PathBuf {
        std::fs::read_dir(root.join("local-summary-models"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().ends_with(".partial"))
            .unwrap()
            .path()
    }
    #[test]
    fn corrupt_retained_prefix_is_not_used_in_a_range_request() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut s = source(&f);
        s.fail_after = Some(17);
        assert!(
            install(
                &f,
                root.path(),
                &Control::default(),
                &mut s,
                u64::MAX,
                &mut |_| {}
            )
            .is_err()
        );
        use std::io::Write;
        let path = staged(root.path()).join(f.catalog.runtime().archive_name.clone());
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .write_all(b"X")
            .unwrap();
        s.opens.clear();
        install(
            &f,
            root.path(),
            &Control::default(),
            &mut s,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(s.opens[0].1, 0, "corrupt prefix must be redownloaded");
    }
    #[test]
    fn checksum_failure_never_creates_an_installed_package() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut s = source(&f);
        s.weights[0] ^= 1;
        let e = install(
            &f,
            root.path(),
            &Control::default(),
            &mut s,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(e.contains("checksum"), "{e}");
        assert!(
            !std::fs::read_dir(root.path().join("local-summary-models"))
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| !e.file_name().to_string_lossy().starts_with('.') && e.path().is_dir())
        );
    }
    #[test]
    fn a_corrupt_installed_file_requires_reverification_and_repair() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut s = source(&f);
        let path = install(
            &f,
            root.path(),
            &Control::default(),
            &mut s,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap();
        std::fs::write(path.join("model.gguf"), b"corrupt").unwrap();
        s.opens.clear();
        let repaired = install(
            &f,
            root.path(),
            &Control::default(),
            &mut s,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(repaired, path);
        assert!(!s.opens.is_empty());
        assert_eq!(std::fs::read(path.join("model.gguf")).unwrap(), f.weights);
    }
    #[test]
    fn overlapping_setup_is_refused_while_actual_source_work_owns_the_package() {
        use std::sync::mpsc;
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        struct Paused {
            inner: Source,
            started: Option<mpsc::Sender<()>>,
            release: mpsc::Receiver<()>,
        }
        impl Transport for Paused {
            fn open(&mut self, spec: &DownloadSpec, offset: u64) -> Result<Response, String> {
                if let Some(tx) = self.started.take() {
                    tx.send(()).unwrap();
                    self.release
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                }
                self.inner.open(spec, offset)
            }
        }
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let mut transport = Paused {
                    inner: source(&f),
                    started: Some(started_tx),
                    release: release_rx,
                };
                install_with(
                    &f.catalog,
                    "fixture",
                    root.path(),
                    &Control::default(),
                    &mut transport,
                    &|_| Ok(u64::MAX),
                    &mut |_| {},
                )
            });
            started_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let mut second = source(&f);
            let error = install(
                &f,
                root.path(),
                &Control::default(),
                &mut second,
                u64::MAX,
                &mut |_| {},
            )
            .unwrap_err();
            assert!(error.contains("owner"), "{error}");
            assert!(second.opens.is_empty());
            release_tx.send(()).unwrap();
            worker.join().unwrap().unwrap();
        });
    }
    #[test]
    fn abrupt_transfer_stop_keeps_a_durable_prefix_and_discards_uncheckpointed_tail() {
        const PREFIX: u64 = 8 << 20;
        let f = fixture_with_weights(vec![42; PREFIX as usize + 131072]);
        let root = tempfile::tempdir().unwrap();
        let archive_bytes = f.archive.len() as u64;
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            install(
                &f,
                root.path(),
                &Control::default(),
                &mut source(&f),
                u64::MAX,
                &mut |p| {
                    if p.phase == Phase::Downloading && p.completed > archive_bytes + PREFIX {
                        panic!("synthetic abrupt owner stop: no cancellation/error checkpoint");
                    }
                },
            )
        }));
        assert!(stopped.is_err());
        let mut resumed = source(&f);
        install(
            &f,
            root.path(),
            &Control::default(),
            &mut resumed,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            resumed.opens,
            vec![("model.gguf".into(), PREFIX)],
            "retain the periodically persisted prefix, trim the unverified tail"
        );
    }
    #[test]
    fn falling_disk_space_stops_an_active_transfer_before_publication() {
        let f = fixture_with_weights(vec![42; (8 << 20) + 131072]);
        let root = tempfile::tempdir().unwrap();
        let available = std::cell::Cell::new(u64::MAX);
        let transferred = std::cell::Cell::new(0);
        let result = install_with(
            &f.catalog,
            "fixture",
            root.path(),
            &Control::default(),
            &mut source(&f),
            &|_| Ok(available.get()),
            &mut |p| {
                if p.phase == Phase::Downloading && p.completed > f.archive.len() as u64 {
                    available.set(0);
                    transferred.set(p.completed);
                }
            },
        );
        assert!(result.unwrap_err().contains("space"));
        assert!(
            transferred.get() < f.archive.len() as u64 + f.weights.len() as u64,
            "stop before consuming all bytes"
        );
        assert!(!staged(root.path()).join("installed.json").exists());
    }

    #[test]
    fn interrupted_replacement_restores_retained_directory_without_network() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut source = source(&f);
        let path = install(
            &f,
            root.path(),
            &Control::default(),
            &mut source,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap();
        let backup = path.with_file_name(format!(
            ".{}.previous",
            path.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::rename(&path, &backup).unwrap();
        source.opens.clear();
        assert_eq!(
            install(
                &f,
                root.path(),
                &Control::default(),
                &mut source,
                u64::MAX,
                &mut |_| {}
            )
            .unwrap(),
            path
        );
        assert!(
            source.opens.is_empty(),
            "recover verified retained package before source access"
        );
        assert!(!backup.exists());
    }
    #[test]
    fn completed_replacement_retires_retained_directory_after_reverification() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut source = source(&f);
        let path = install(
            &f,
            root.path(),
            &Control::default(),
            &mut source,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap();
        let backup = path.with_file_name(format!(
            ".{}.previous",
            path.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::create_dir(&backup).unwrap();
        std::fs::copy(path.join("installed.json"), backup.join("installed.json")).unwrap();
        source.opens.clear();
        install(
            &f,
            root.path(),
            &Control::default(),
            &mut source,
            u64::MAX,
            &mut |_| {},
        )
        .unwrap();
        assert!(source.opens.is_empty());
        assert!(
            !backup.exists(),
            "retire crash-left backup only after installed content verifies"
        );
    }
    #[test]
    fn resume_refuses_wrong_range_or_length_before_consuming_the_response() {
        for bad_length in [false, true] {
            let f = fixture();
            let root = tempfile::tempdir().unwrap();
            let mut s = source(&f);
            s.fail_after = Some(17);
            assert!(
                install(
                    &f,
                    root.path(),
                    &Control::default(),
                    &mut s,
                    u64::MAX,
                    &mut |_| {}
                )
                .is_err()
            );
            struct Wrong {
                inner: Source,
                bad_length: bool,
            }
            impl Transport for Wrong {
                fn open(&mut self, spec: &DownloadSpec, offset: u64) -> Result<Response, String> {
                    let mut r = self.inner.open(spec, offset)?;
                    if self.bad_length {
                        r.length = Some(1);
                    } else {
                        r.range = Some((offset + 1, spec.bytes() - 1, spec.bytes()));
                    }
                    Ok(r)
                }
            }
            let mut wrong = Wrong {
                inner: source(&f),
                bad_length,
            };
            let error = install_with(
                &f.catalog,
                "fixture",
                root.path(),
                &Control::default(),
                &mut wrong,
                &|_| Ok(u64::MAX),
                &mut |_| {},
            )
            .unwrap_err();
            assert!(error.contains("range mismatch"), "{error}");
            assert_eq!(
                std::fs::metadata(staged(root.path()).join(&f.catalog.runtime().archive_name))
                    .unwrap()
                    .len(),
                17
            );
        }
    }
    #[test]
    fn server_ignoring_range_restarts_and_verifies_the_whole_artifact() {
        let f = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut s = source(&f);
        s.fail_after = Some(17);
        assert!(
            install(
                &f,
                root.path(),
                &Control::default(),
                &mut s,
                u64::MAX,
                &mut |_| {}
            )
            .is_err()
        );
        struct IgnoresRange(Source);
        impl Transport for IgnoresRange {
            fn open(&mut self, spec: &DownloadSpec, _: u64) -> Result<Response, String> {
                self.0.open(spec, 0)
            }
        }
        let path = install_with(
            &f.catalog,
            "fixture",
            root.path(),
            &Control::default(),
            &mut IgnoresRange(source(&f)),
            &|_| Ok(u64::MAX),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(path.join("model.gguf")).unwrap(), f.weights);
    }
}
