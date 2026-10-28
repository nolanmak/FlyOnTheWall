//! Explicit, checksummed model setup. Run blocking installation on an owned worker.
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
const BUFFER: usize = 64 * 1024;
const HEADROOM: u64 = 64 * 1024 * 1024;

/// Cross-process admission shared by capture and exclusive model setup.
#[derive(Debug)]
pub struct WorkLease {
    file: File,
    identity: PathBuf,
}
impl WorkLease {
    /// Hold while capture preparation, pumping or native teardown still runs.
    pub fn capture(sessions: &Path) -> Result<Self, String> {
        let sessions = std::path::absolute(sessions).map_err(io_error)?;
        Self::open(sessions.parent().unwrap_or(&sessions), false)
    }
    pub(crate) fn quarantine(lease: Arc<Self>) {
        // One retained owner per library, rather than another leaked descriptor
        // for every failed Stop. Process exit releases unresolved exclusion.
        static OWNERS: std::sync::OnceLock<
            std::sync::Mutex<std::collections::BTreeMap<PathBuf, Arc<WorkLease>>>,
        > = std::sync::OnceLock::new();
        OWNERS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(lease.identity.clone())
            .or_insert(lease);
    }
    fn setup(data_root: &Path) -> Result<Self, String> {
        Self::open(data_root, true)
    }
    fn open(data_root: &Path, exclusive: bool) -> Result<Self, String> {
        let absolute = std::path::absolute(data_root).map_err(io_error)?;
        let data_root = match absolute.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                private_dir(&absolute)?;
                absolute.canonicalize().map_err(io_error)?
            }
            Err(error) => return Err(io_error(error)),
        };
        private_dir(&data_root)?;
        let file = options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(data_root.join("model-work.lock"))
            .map_err(io_error)?;
        if !file.metadata().map_err(io_error)?.is_file() {
            return Err("Capture/setup admission requires an ordinary lock file".into());
        }
        let locked = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        locked.map_err(|_| if exclusive {
            "Capture or another model setup still owns this library; finish it before installing models".to_owned()
        } else {
            "Model setup is running; finish or cancel it before starting a recording".to_owned()
        })?;
        Ok(Self {
            file,
            identity: data_root,
        })
    }
}
impl Drop for WorkLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// A single installation's cancellation/publication boundary.
#[derive(Clone, Debug, Default)]
pub struct Control(Arc<AtomicU8>);
impl Control {
    /// Request cancellation before publication. False if already requested or committing.
    pub fn cancel(&self) -> bool {
        self.0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(crate) fn check(&self) -> Result<(), String> {
        match self.0.load(Ordering::Acquire) {
            0 => Ok(()),
            1 => Err("Model setup cancelled; previous model preserved".into()),
            _ => Err("This model setup control has already entered publication".into()),
        }
    }
    /// After one artifact of a set has been published, admit cancellation
    /// again for the next. Only a published (committing) control reopens.
    fn reopen(&self) -> Result<(), String> {
        self.0
            .compare_exchange(2, 0, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| "Model setup control was not between publications".into())
    }
    pub(crate) fn commit(&self) -> Result<(), String> {
        self.0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| {
                "Model setup cancelled before publication; previous model preserved".into()
            })
    }
}
/// The source is explicit; ordinary recording never invokes this installer.
pub enum Source {
    /// Fetch the known catalog artifact over HTTPS.
    Download,
    /// Import a local file whose size and checksum must match the catalog.
    Import(PathBuf),
}
/// Observable installation stage. Verified weights still require native Start preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Inspect an existing model before deciding whether source access is needed.
    Checking,
    /// Stream into a separate private partial file.
    Transferring,
    /// Check the completed transfer before touching the destination.
    Verifying,
    /// Atomically publish a synced, checksum-verified file.
    Installing,
    /// The file is verified; native readiness remains a separate operation.
    Verified,
}
/// Metadata-only progress; no audio, model contents or credentials.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    /// Current stage.
    pub phase: Phase,
    /// Bytes transferred or verified.
    pub bytes: u64,
    /// Pinned expected size.
    pub total: u64,
}
#[derive(Clone)]
struct Artifact {
    name: String,
    revision: String,
    sha256: String,
    bytes: u64,
    license: String,
}
impl Artifact {
    fn known(model: &crate::stt::KnownModel) -> Self {
        Self {
            name: model.name.into(),
            revision: model.revision.into(),
            sha256: model.sha256.into(),
            bytes: model.bytes,
            license: model.license.into(),
        }
    }
    fn validate(&self) -> Result<(), String> {
        let hex = |s: &str, n| {
            s.len() == n
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if self.name.is_empty()
            || self.name.len() > 80
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
            || self.name == "."
            || self.name == ".."
            || !hex(&self.revision, 40)
            || !hex(&self.sha256, 64)
            || self.license != "MIT"
            || self.bytes == 0
            || self.bytes > 2 * 1024 * 1024 * 1024
        {
            return Err(
                "Model catalog identity, revision, hash, license or size is invalid".into(),
            );
        }
        Ok(())
    }
}
fn io_error(e: std::io::Error) -> String {
    format!("Model setup I/O failed: {e}")
}
fn options() -> OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut out = OpenOptions::new();
        out.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        out
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
    }
}

pub(crate) fn private_dir(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_dir() => {
            return Err("Model setup directory must be an ordinary directory".into());
        }
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error(e)),
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io_error)
}
fn ordinary(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(Some(meta)),
        Ok(_) => Err("Model input/destination must be an ordinary file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(e)),
    }
}
fn digest(hash: Sha256) -> String {
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
fn verified(path: &Path, artifact: &Artifact, control: &Control) -> Result<bool, String> {
    let Some(meta) = ordinary(path)? else {
        return Ok(false);
    };
    if meta.len() != artifact.bytes {
        return Ok(false);
    }
    let mut file = options().read(true).open(path).map_err(io_error)?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; BUFFER];
    loop {
        control.check()?;
        let n = file.read(&mut buf).map_err(io_error)?;
        control.check()?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).ok_or("Model size overflow")?;
        if bytes > artifact.bytes {
            return Ok(false);
        }
        hash.update(&buf[..n]);
    }
    Ok(bytes == artifact.bytes && digest(hash) == artifact.sha256)
}
struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn install_with_progress(
    root: &Path,
    artifact: &Artifact,
    control: &Control,
    available: impl FnOnce(&Path) -> Result<u64, String>,
    open: impl FnOnce() -> Result<Box<dyn Read + Send>, String>,
    mut progress: impl FnMut(Progress),
) -> Result<PathBuf, String> {
    artifact.validate()?;
    control.check()?;
    // Declared before source/client/file owners, so admission survives their teardown.
    let _lease = WorkLease::setup(root)?;
    let dir = root.join("models");
    private_dir(&dir)?;
    let path = dir.join(crate::stt::model_file_name(&artifact.name));
    let mut report = |phase, bytes| {
        progress(Progress {
            phase,
            bytes,
            total: artifact.bytes,
        })
    };
    report(Phase::Checking, 0);
    if verified(&path, artifact, control)? {
        control.commit()?;
        report(Phase::Verified, artifact.bytes);
        return Ok(path);
    }
    let partial = dir.join(format!(".ggml-{}.bin.part", artifact.name));
    if ordinary(&partial)?.is_some() {
        std::fs::remove_file(&partial).map_err(io_error)?;
    }
    let required = artifact
        .bytes
        .checked_add(HEADROOM)
        .ok_or("Model size overflow")?;
    if available(&dir)? < required {
        return Err(format!(
            "Not enough free space: need {required} bytes including installation headroom; previous model preserved"
        ));
    }
    control.check()?;
    let partial = Partial(partial);
    let mut target = options()
        .write(true)
        .create_new(true)
        .open(&partial.0)
        .map_err(io_error)?;
    report(Phase::Transferring, 0);
    let mut source = open()?;
    let mut buf = [0u8; BUFFER];
    let mut hash = Sha256::new();
    let mut copied = 0u64;
    loop {
        control.check()?;
        let n = source.read(&mut buf).map_err(io_error)?;
        control.check()?;
        if n == 0 {
            break;
        }
        copied = copied.checked_add(n as u64).ok_or("Model size overflow")?;
        if copied > artifact.bytes {
            return Err("Model source exceeds its pinned size; previous model preserved".into());
        }
        target.write_all(&buf[..n]).map_err(io_error)?;
        hash.update(&buf[..n]);
        report(Phase::Transferring, copied);
    }
    report(Phase::Verifying, copied);
    if copied != artifact.bytes || digest(hash) != artifact.sha256 {
        return Err(
            "Model size or SHA-256 differs from the pinned catalog; previous model preserved"
                .into(),
        );
    }
    target.flush().map_err(io_error)?;
    target.sync_all().map_err(io_error)?;
    drop(target);
    control.check()?;
    control.commit()?;
    report(Phase::Installing, copied);
    std::fs::rename(&partial.0, &path).map_err(io_error)?;
    #[cfg(unix)]
    File::open(&dir)
        .and_then(|file| file.sync_all())
        .map_err(|_| {
            "Verified model replaced, but directory sync failed; verify it again after restart"
                .to_owned()
        })?;
    report(Phase::Verified, copied);
    Ok(path)
}
/// Install a Whisper model together with the speech detector that gates it
/// (#242), so a model that can transcribe is never on disk without the gate
/// local capture requires. `open` is asked for each artifact's source only
/// when that artifact is not already installed and verified.
fn install_with_detector(
    root: &Path,
    model: &Artifact,
    detector: &Artifact,
    control: &Control,
    available: impl Fn(&Path) -> Result<u64, String>,
    mut open: impl FnMut(&Artifact) -> Result<Box<dyn Read + Send>, String>,
    progress: impl FnMut(Progress),
) -> Result<PathBuf, String> {
    let mut progress = progress;
    // The detector first: it is small, so a failure costs nothing, and a model
    // is never left installed without the gate it needs. Both share the one
    // control, so Cancel stops whichever is transferring; once the detector
    // is published, the control reopens for the model's own boundary.
    install_with_progress(
        root,
        detector,
        control,
        &available,
        || open(detector),
        &mut progress,
    )
    .map_err(|error| {
        format!(
            "The speech detector {} could not be installed, so the model was not either: {error}",
            detector.name
        )
    })?;
    control.reopen()?;
    install_with_progress(root, model, control, &available, || open(model), progress)
}
/// A helper model such as Parakeet Unified (#244): its speech detector first,
/// then the helper's own install of the pinned revision, under the same setup
/// admission as any model, so it never runs beside a capture. Only the helper
/// touches the network for it, and only here.
fn install_helper_model(
    root: &Path,
    model: &crate::speechd::HelperModel,
    source: Source,
    control: &Control,
    mut progress: impl FnMut(Progress),
    detector: impl FnOnce(&mut dyn FnMut(Progress)) -> Result<PathBuf, String>,
    launch: impl FnOnce() -> Result<crate::speechd::Launch, String>,
) -> Result<PathBuf, String> {
    use fotw_speechd_client::message::ProgressPhase;
    control.check()?;
    let launch = launch()?;
    detector(&mut progress).map_err(|error| {
        format!(
            "The speech detector {} could not be installed, so {} was not either: {error}",
            crate::stt::VAD_MODEL.name,
            model.id
        )
    })?;
    control.check()?;
    // Declared before the helper, so admission outlives it.
    let _lease = WorkLease::setup(root)?;
    let total = model.bytes;
    progress(Progress {
        phase: Phase::Checking,
        bytes: 0,
        total,
    });
    let from = match source {
        Source::Import(path) => Some(path),
        Source::Download => None,
    };
    let revision = crate::speechd::install_model(
        &launch,
        root,
        model,
        from.as_deref(),
        &|| control.check().is_err(),
        &mut |p| {
            progress(Progress {
                phase: match p.phase {
                    ProgressPhase::Download => Phase::Transferring,
                    ProgressPhase::Verify => Phase::Verifying,
                    _ => Phase::Installing,
                },
                bytes: p.bytes,
                total: p.total,
            });
        },
    )?;
    // The helper has already selected it; a late cancel cannot undo that.
    let _ = control.commit();
    progress(Progress {
        phase: Phase::Verified,
        bytes: total,
        total,
    });
    Ok(crate::speechd::models_root(root)
        .join(model.id)
        .join(revision))
}
#[cfg(test)]
fn install_from(
    root: &Path,
    artifact: &Artifact,
    control: &Control,
    available: impl FnOnce(&Path) -> Result<u64, String>,
    open: impl FnOnce() -> Result<Box<dyn Read + Send>, String>,
) -> Result<PathBuf, String> {
    install_with_progress(root, artifact, control, available, open, |_| {})
}
/// Download or import one known model. Never selects an ASR policy or opens the library.
///
/// Call on a blocking worker. Cancellation is observed between reads; an outstanding
/// OS/network operation retains exclusive admission until it actually returns.
pub fn install(
    root: &Path,
    name: &str,
    source: Source,
    control: &Control,
    progress: impl FnMut(Progress),
) -> Result<PathBuf, String> {
    let detector = &crate::stt::VAD_MODEL;
    let download = |model: &crate::stt::KnownModel| -> Result<Box<dyn Read + Send>, String> {
        // No credentials, proxy discovery, cookies, metadata lookup or cloud ASR.
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .https_only(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(900))
            .redirect(reqwest::redirect::Policy::limited(5))
            .referer(false)
            .build()
            .map_err(|_| "Cannot initialize the model download client")?;
        download_response(&client, &model.url(), model.bytes)
    };
    if let Some(model) = crate::speechd::helper_model(name) {
        // The helper's models are gated by the same detector Whisper is. A
        // download brings it; an import brings only the named model, as for
        // Whisper.
        let importing = matches!(source, Source::Import(_));
        return install_helper_model(
            root,
            model,
            source,
            control,
            progress,
            |progress| {
                if importing {
                    crate::stt::verify_vad_model(root)
                } else {
                    // Cancel stops the detector's transfer too; once it is
                    // published the control reopens for the model.
                    let path = install_with_progress(
                        root,
                        &Artifact::known(detector),
                        control,
                        free_space,
                        || download(detector),
                        progress,
                    )?;
                    control.reopen()?;
                    Ok(path)
                }
            },
            crate::speechd::Launch::bundled,
        );
    }
    let known =
        crate::stt::catalog_model(name).ok_or("Choose a model from `fotwd stt models list`")?;
    if known.name == detector.name {
        return install_with_progress(
            root,
            &Artifact::known(known),
            control,
            free_space,
            || match source {
                Source::Import(path) => open_import(&path),
                Source::Download => download(known),
            },
            progress,
        );
    }
    // A Whisper model comes with the speech detector local capture requires.
    let mut source = Some(source);
    install_with_detector(
        root,
        &Artifact::known(known),
        &Artifact::known(detector),
        control,
        free_space,
        |artifact| {
            if artifact.name == detector.name {
                return match source {
                    Some(Source::Download) => download(detector),
                    _ => Err(format!(
                        "it is not installed and an import brings only the Whisper model; \
                         import it with `fotwd stt models import {0} <file>` or download it with \
                         `fotwd stt models install {0}`",
                        detector.name
                    )),
                };
            }
            match source.take() {
                Some(Source::Import(path)) => open_import(&path),
                Some(Source::Download) => download(known),
                None => Err("Model source was already used".into()),
            }
        },
        progress,
    )
}
fn open_import(path: &Path) -> Result<Box<dyn Read + Send>, String> {
    if !path.is_absolute() {
        return Err("Choose an absolute import path".into());
    }
    // Cache snapshots may be symlinks. Only the read source is resolved;
    // publication still verifies every byte and refuses destination symlinks.
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("Imported model is missing".into());
        }
        Err(error) => return Err(io_error(error)),
    };
    if ordinary(&path)?.is_none() {
        return Err("Imported model is missing".into());
    }
    Ok(Box::new(options().read(true).open(path).map_err(io_error)?))
}
fn download_response(
    client: &reqwest::blocking::Client,
    url: &str,
    expected: u64,
) -> Result<Box<dyn Read + Send>, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|_| "Model download failed or timed out; retry or import the pinned file")?;
    if response.status() != reqwest::StatusCode::OK
        || response
            .content_length()
            .is_some_and(|size| size != expected)
    {
        return Err("Model server refused the file or reported an unexpected size".into());
    }
    Ok(Box::new(response))
}

#[cfg(unix)]
fn free_space(path: &Path) -> Result<u64, String> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "Invalid model directory")?;
    let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: both pointers remain valid for this call; output is read only on success.
    if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    // SAFETY: statvfs initialized the complete output after returning success.
    let info = unsafe { info.assume_init() };
    u64::try_from(u128::from(info.f_bavail) * u128::from(info.f_frsize))
        .map_err(|_| "Free-space result exceeds supported range".into())
}
#[cfg(not(unix))]
fn free_space(_: &Path) -> Result<u64, String> {
    Err("Model setup disk checks are currently supported on Unix platforms".into())
}
// Attached to every opened tap, including supervised replacements.
pub(crate) use crate::model_capture::{lease_platform, lease_tap};

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    const AUDIO_FREE_BYTES: u64 = 1 << 30;
    fn spec() -> Artifact {
        Artifact {
            name: "fixture".into(),
            revision: "0".repeat(40),
            sha256: Sha256::digest(b"fixture model")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            bytes: 13,
            license: "MIT".into(),
        }
    }
    fn reader(bytes: &[u8]) -> Box<dyn Read + Send> {
        Box::new(std::io::Cursor::new(bytes.to_vec()))
    }
    #[cfg(unix)]
    #[test]
    fn offline_cache_symlink_import_verifies_content_and_rejects_bad_targets() {
        let source = tempfile::tempdir().unwrap();
        let blob = source.path().join("blob");
        let snapshot = source.path().join("snapshot.bin");
        std::fs::write(&blob, b"fixture model").unwrap();
        std::os::unix::fs::symlink(&blob, &snapshot).unwrap();
        let root = tempfile::tempdir().unwrap();
        let target = install_from(
            root.path(),
            &spec(),
            &Control::default(),
            |_| Ok(u64::MAX),
            || open_import(&snapshot),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"fixture model");
        std::fs::write(&target, b"previous").unwrap();
        std::fs::write(&blob, b"corrupt model").unwrap();
        assert!(
            install_from(
                root.path(),
                &spec(),
                &Control::default(),
                |_| Ok(u64::MAX),
                || open_import(&snapshot)
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"previous");
        std::fs::remove_file(&blob).unwrap();
        assert!(open_import(&snapshot).err().unwrap().contains("missing"));
        std::fs::create_dir(&blob).unwrap();
        assert!(open_import(&snapshot).is_err());
    }
    #[test]
    fn corrupt_or_truncated_source_never_replaces_the_existing_model() {
        for bad in [
            b"bad".as_slice(),
            b"wrong content".as_slice(),
            b"fixture modelEXTRA".as_slice(),
        ] {
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("models/ggml-fixture.bin");
            std::fs::create_dir(target.parent().unwrap()).unwrap();
            std::fs::write(&target, b"previous").unwrap();
            assert!(
                install_from(
                    root.path(),
                    &spec(),
                    &Control::default(),
                    |_| Ok(AUDIO_FREE_BYTES),
                    || Ok(reader(bad))
                )
                .is_err()
            );
            assert_eq!(std::fs::read(target).unwrap(), b"previous");
        }
    }
    #[test]
    fn disk_refusal_and_cancellation_happen_before_source_access() {
        let root = tempfile::tempdir().unwrap();
        let control = Control::default();
        assert!(
            install_from(
                root.path(),
                &spec(),
                &control,
                |_| Ok(0),
                || panic!("disk refusal opened the source")
            )
            .is_err()
        );
        assert!(control.cancel());
        assert!(
            install_from(
                root.path(),
                &spec(),
                &control,
                |_| Ok(AUDIO_FREE_BYTES),
                || panic!("cancelled setup opened the source")
            )
            .is_err()
        );
    }
    #[test]
    fn a_verified_install_restarts_offline_without_opening_its_source() {
        let root = tempfile::tempdir().unwrap();
        let path = install_from(
            root.path(),
            &spec(),
            &Control::default(),
            |_| Ok(AUDIO_FREE_BYTES),
            || Ok(reader(b"fixture model")),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"fixture model");
        assert_eq!(
            install_from(
                root.path(),
                &spec(),
                &Control::default(),
                |_| Ok(0),
                || panic!("verified model restarted a download")
            )
            .unwrap(),
            path
        );
    }
    fn artifact(name: &str, bytes: &[u8]) -> Artifact {
        Artifact {
            name: name.into(),
            revision: "0".repeat(40),
            sha256: Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            bytes: bytes.len() as u64,
            license: "MIT".into(),
        }
    }
    #[test]
    fn a_whisper_model_is_installed_with_its_speech_detector() {
        let root = tempfile::tempdir().unwrap();
        let model = artifact("fixture-asr", b"fixture asr weights");
        let detector = artifact("fixture-vad", b"fixture vad weights");
        let path = install_with_detector(
            root.path(),
            &model,
            &detector,
            &Control::default(),
            |_| Ok(AUDIO_FREE_BYTES),
            |a| {
                Ok(reader(if a.name == "fixture-asr" {
                    b"fixture asr weights"
                } else {
                    b"fixture vad weights"
                }))
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(path, root.path().join("models/ggml-fixture-asr.bin"));
        let detector = root.path().join("models/ggml-fixture-vad.bin");
        assert!(
            detector.is_file(),
            "the detector was not installed with the model"
        );
        assert_eq!(std::fs::read(detector).unwrap(), b"fixture vad weights");
    }
    #[test]
    fn a_detector_that_fails_verification_stops_setup_before_the_model() {
        let root = tempfile::tempdir().unwrap();
        let model = artifact("fixture-asr", b"fixture asr weights");
        let detector = artifact("fixture-vad", b"fixture vad weights");
        let error = install_with_detector(
            root.path(),
            &model,
            &detector,
            &Control::default(),
            |_| Ok(AUDIO_FREE_BYTES),
            |a| {
                assert_eq!(a.name, "fixture-vad", "model fetched before the detector");
                Ok(reader(b"tampered vad weight"))
            },
            |_| {},
        )
        .unwrap_err();
        assert!(error.contains("speech detector"), "{error}");
        assert!(!root.path().join("models/ggml-fixture-asr.bin").exists());
    }
    #[test]
    fn cancelling_during_the_detector_transfer_publishes_nothing() {
        struct Cancelling(Control, Box<dyn Read + Send>);
        impl Read for Cancelling {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.cancel();
                self.1.read(buf)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let model = artifact("fixture-asr", b"fixture asr weights");
        let detector = artifact("fixture-vad", b"fixture vad weights");
        let control = Control::default();
        let cancel = control.clone();
        let error = install_with_detector(
            root.path(),
            &model,
            &detector,
            &control,
            |_| Ok(AUDIO_FREE_BYTES),
            |a| {
                assert_eq!(a.name, "fixture-vad", "cancelled setup opened the model");
                Ok(Box::new(Cancelling(
                    cancel.clone(),
                    reader(b"fixture vad weights"),
                )))
            },
            |_| {},
        )
        .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(
            !root.path().join("models/ggml-fixture-vad.bin").exists(),
            "Cancel during the detector download did not stop it"
        );
    }
    #[test]
    fn a_cancelled_model_after_its_detector_still_cancels() {
        let root = tempfile::tempdir().unwrap();
        let model = artifact("fixture-asr", b"fixture asr weights");
        let detector = artifact("fixture-vad", b"fixture vad weights");
        let control = Control::default();
        let cancel = control.clone();
        let error = install_with_detector(
            root.path(),
            &model,
            &detector,
            &control,
            |_| Ok(AUDIO_FREE_BYTES),
            |a| {
                if a.name == "fixture-asr" {
                    assert!(
                        cancel.cancel(),
                        "the model's transfer must still be cancellable"
                    );
                }
                Ok(reader(if a.name == "fixture-asr" {
                    b"fixture asr weights"
                } else {
                    b"fixture vad weights"
                }))
            },
            |_| {},
        )
        .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(!root.path().join("models/ggml-fixture-asr.bin").exists());
    }
    #[test]
    fn parakeet_installs_through_the_helper_after_its_speech_detector() {
        let root = tempfile::tempdir().unwrap();
        let mut phases = Vec::new();
        let mut detector_first = false;
        let path = install_helper_model(
            root.path(),
            &crate::speechd::PARAKEET,
            Source::Download,
            &Control::default(),
            |p| phases.push(p.phase),
            |_| {
                detector_first = true;
                Ok(root.path().join("models/ggml-fixture-vad.bin"))
            },
            || Ok(crate::speechd::testing::fake_launch()),
        )
        .unwrap();
        assert!(detector_first);
        assert!(crate::speechd::present(root.path()));
        assert_eq!(
            path,
            crate::speechd::models_root(root.path())
                .join(crate::speechd::ENGINE)
                .join(crate::speechd::REVISION)
        );
        assert_eq!(phases.first(), Some(&Phase::Checking));
        assert_eq!(phases.last(), Some(&Phase::Verified));
    }
    #[test]
    fn a_parakeet_install_without_its_detector_never_starts_the_helper() {
        let root = tempfile::tempdir().unwrap();
        let error = install_helper_model(
            root.path(),
            &crate::speechd::PARAKEET,
            Source::Import(root.path().join("parakeet")),
            &Control::default(),
            |_| {},
            |_| Err("missing at /fixture".into()),
            || Ok(crate::speechd::testing::fake_launch()),
        )
        .unwrap_err();
        assert!(error.contains("speech detector"), "{error}");
        assert!(!crate::speechd::present(root.path()));
    }
    #[test]
    fn a_parakeet_install_waits_for_capture_to_release_the_library() {
        let root = tempfile::tempdir().unwrap();
        let _capture = WorkLease::capture(&root.path().join("sessions")).unwrap();
        let error = install_helper_model(
            root.path(),
            &crate::speechd::PARAKEET,
            Source::Download,
            &Control::default(),
            |_| {},
            |_| Ok(PathBuf::new()),
            || Ok(crate::speechd::testing::fake_launch()),
        )
        .unwrap_err();
        assert!(error.contains("Capture"), "{error}");
        assert!(!crate::speechd::present(root.path()));
    }
    #[test]
    fn capture_and_setup_are_exclusive_across_independent_file_owners() {
        let root = tempfile::tempdir().unwrap();
        let a = WorkLease::capture(&root.path().join("sessions")).unwrap();
        let b = WorkLease::capture(&root.path().join("sessions")).unwrap();
        assert!(WorkLease::setup(root.path()).is_err());
        drop(a);
        assert!(WorkLease::setup(root.path()).is_err());
        drop(b);
        let setup = WorkLease::setup(root.path()).unwrap();
        assert!(WorkLease::capture(&root.path().join("sessions")).is_err());
        assert!(WorkLease::setup(root.path()).is_err());
        drop(setup);
        assert!(WorkLease::capture(&root.path().join("sessions")).is_ok());
    }
}

#[cfg(test)]
mod capture_gate_tests {
    use super::*;
    use fotw_audio::{AudioTap, FrameSink, SampleFormat, StreamFormat, TapError, TapId};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    struct Tap {
        id: TapId,
        starts: Arc<AtomicUsize>,
        closing: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    }
    impl AudioTap for Tap {
        fn id(&self) -> &TapId {
            &self.id
        }
        fn format(&self) -> StreamFormat {
            StreamFormat::new(16000, 1, SampleFormat::F32)
        }
        fn format_is_authoritative(&self) -> bool {
            true
        }
        fn start(&mut self, _sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(self.format())
        }
        fn stop(&mut self) -> Result<(), TapError> {
            if let Some((began, release)) = self.closing.take() {
                began.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Ok(())
        }
    }
    #[tokio::test]
    async fn setup_refuses_session_capture_before_any_tap_is_started() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let _setup = WorkLease::setup(root.path()).unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let tap = Box::new(Tap {
            id: TapId::system_default(),
            starts: starts.clone(),
            closing: None,
        });
        let result = crate::session::run(
            &sessions,
            tap,
            None,
            crate::session::Transcription::Disabled,
            Duration::ZERO,
        )
        .await;
        assert!(
            result.is_err(),
            "session started capture during exclusive model setup"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn a_blocked_native_stop_keeps_setup_excluded_until_actual_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(WorkLease::capture(&root.path().join("sessions")).unwrap());
        let (began, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let mut tap = lease_tap(
            Box::new(Tap {
                id: TapId::system_default(),
                starts: Default::default(),
                closing: Some((began, gate)),
            }),
            lease.clone(),
        );
        drop(lease);
        let worker = std::thread::spawn(move || {
            tap.stop().unwrap();
            drop(tap);
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let blocked = WorkLease::setup(root.path()).is_err();
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            blocked,
            "detached native Stop released setup admission too early"
        );
        assert!(WorkLease::setup(root.path()).is_ok());
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use std::sync::mpsc;
    fn spec() -> Artifact {
        Artifact {
            name: "transfer".into(),
            revision: "1".repeat(40),
            sha256: Sha256::digest(b"fixture model")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            bytes: 13,
            license: "MIT".into(),
        }
    }
    fn old(root: &Path) -> PathBuf {
        let path = root.join("models/ggml-transfer.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"previous").unwrap();
        path
    }
    #[test]
    fn interrupted_http_size_status_and_body_timeout_preserve_previous_weights() {
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nfixture model".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nwrong size body".as_slice(),
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nshort".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n11\r\nfixture modelMORE\r\n0\r\n\r\n".as_slice(),
        ] {
            let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let url=format!("http://{}/model",listener.local_addr().unwrap());
            let response=response.to_vec();let good=response.ends_with(b"fixture model");
            let worker=std::thread::spawn(move||{let(mut socket,_)=listener.accept().unwrap();let mut request=[0;4096];socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();assert!(socket.read(&mut request).unwrap()>0);socket.write_all(&response).unwrap();});
            let root=tempfile::tempdir().unwrap();let target=old(root.path());
            let client=reqwest::blocking::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(3)).build().unwrap();
            let result=install_from(root.path(),&spec(),&Control::default(), |_|Ok(u64::MAX), ||download_response(&client,&url,13));
            worker.join().unwrap();
            assert_eq!(result.is_ok(),good);
            assert_eq!(std::fs::read(target).unwrap(),if good {b"fixture model".as_slice()}else{b"previous".as_slice()});
            assert!(!root.path().join("models/.ggml-transfer.bin.part").exists());
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/stall", listener.local_addr().unwrap());
        let (release, gate) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n")
                .unwrap();
            let _ = gate.recv_timeout(Duration::from_secs(3));
        });
        let root = tempfile::tempdir().unwrap();
        let target = old(root.path());
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let result = install_from(
            root.path(),
            &spec(),
            &Control::default(),
            |_| Ok(u64::MAX),
            || download_response(&client, &url, 13),
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"previous");
    }
    #[test]
    fn progressing_download_can_outlive_the_blocking_per_operation_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/progress", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\n")
                .unwrap();
            for byte in b"fixture model" {
                std::thread::sleep(Duration::from_millis(150));
                if socket.write_all(&[*byte]).is_err() {
                    break;
                }
            }
        });
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let began = std::time::Instant::now();
        let result = install_from(
            root.path(),
            &spec(),
            &Control::default(),
            |_| Ok(u64::MAX),
            || download_response(&client, &url, 13),
        );
        worker.join().unwrap();
        assert!(began.elapsed() > Duration::from_secs(1));
        let path = result.expect("continuous progress must not hit an aggregate deadline");
        assert_eq!(std::fs::read(path).unwrap(), b"fixture model");
    }
    #[test]
    fn cancellation_of_a_blocked_source_retains_admission_and_fences_late_publication() {
        struct Blocked(mpsc::Sender<()>, mpsc::Receiver<()>);
        impl Read for Blocked {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.send(()).unwrap();
                self.1.recv_timeout(Duration::from_secs(3)).unwrap();
                buf[..13].copy_from_slice(b"fixture model");
                Ok(13)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let target = old(root.path());
        let path = root.path().to_owned();
        let (began, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let control = Control::default();
        let owned = control.clone();
        let worker = std::thread::spawn(move || {
            install_from(
                &path,
                &spec(),
                &owned,
                |_| Ok(u64::MAX),
                || Ok(Box::new(Blocked(began, gate))),
            )
        });
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(control.cancel());
        let blocked = WorkLease::capture(&root.path().join("sessions")).is_err();
        release.send(()).unwrap();
        let result = worker.join().unwrap();
        assert!(blocked);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"previous");
        assert!(WorkLease::capture(&root.path().join("sessions")).is_ok());
        assert!(!root.path().join("models/.ggml-transfer.bin.part").exists());
    }
    #[test]
    fn publication_is_the_explicit_cancellation_boundary_and_progress_is_real() {
        let root = tempfile::tempdir().unwrap();
        let control = Control::default();
        let mut events = Vec::new();
        install_with_progress(
            root.path(),
            &spec(),
            &control,
            |_| Ok(u64::MAX),
            || Ok(Box::new(std::io::Cursor::new(b"fixture model"))),
            |progress| {
                if progress.phase == Phase::Installing {
                    assert!(!control.cancel());
                }
                events.push(progress);
            },
        )
        .unwrap();
        assert!(
            events
                .iter()
                .any(|p| p.phase == Phase::Transferring && p.bytes == 13)
        );
        assert_eq!(events.last().unwrap().phase, Phase::Verified);
        assert!(events.iter().all(|p| p.bytes <= p.total));
    }
}

#[cfg(test)]
mod crash_tests {
    use super::*;
    #[test]
    #[ignore = "private child process for the owned model setup crash fixture"]
    fn model_setup_crash_child() {
        let root = PathBuf::from(std::env::var_os("FOTW_MODEL_SETUP_CRASH_ROOT").unwrap());
        assert_eq!(
            std::fs::read(root.join("fixture-owner")).unwrap(),
            b"owned model setup fixture v1"
        );
        struct CrashSource(bool);
        impl Read for CrashSource {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    loop {
                        std::thread::park()
                    }
                }
                self.0 = true;
                buf[..13].copy_from_slice(b"fixture model");
                Ok(13)
            }
        }
        let spec = Artifact {
            name: "crash".into(),
            revision: "0".repeat(40),
            sha256: Sha256::digest(b"fixture model")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            bytes: 13,
            license: "MIT".into(),
        };
        let _ = install_with_progress(
            &root,
            &spec,
            &Control::default(),
            |_| Ok(u64::MAX),
            || Ok(Box::new(CrashSource(false))),
            |p| {
                if p.phase == Phase::Transferring && p.bytes == 13 {
                    std::fs::write(root.join("ready.tmp"), b"ready").unwrap();
                    std::fs::rename(root.join("ready.tmp"), root.join("ready")).unwrap();
                }
            },
        );
    }
    #[test]
    fn a_killed_installer_preserves_old_weights_and_next_explicit_retry_reclaims_its_partial() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("fixture-owner"),
            b"owned model setup fixture v1",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("models")).unwrap();
        let target = root.path().join("models/ggml-crash.bin");
        std::fs::write(&target, b"previous").unwrap();
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "model_setup::crash_tests::model_setup_crash_child",
                    "--ignored",
                ])
                .env("FOTW_MODEL_SETUP_CRASH_ROOT", root.path())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !root.path().join("ready").exists() {
            assert!(child.0.try_wait().unwrap().is_none());
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(WorkLease::capture(&root.path().join("sessions")).is_err());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"previous");
        assert!(root.path().join("models/.ggml-crash.bin.part").exists());
        let spec = Artifact {
            name: "crash".into(),
            revision: "0".repeat(40),
            sha256: Sha256::digest(b"fixture model")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            bytes: 13,
            license: "MIT".into(),
        };
        install_from(
            root.path(),
            &spec,
            &Control::default(),
            |_| Ok(u64::MAX),
            || Ok(Box::new(std::io::Cursor::new(b"fixture model"))),
        )
        .unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"fixture model");
        assert!(!root.path().join("models/.ggml-crash.bin.part").exists());
    }
}

#[cfg(test)]
mod custom_root_tests {
    use super::*;
    use fotw_audio::{AudioTap, FrameSink, SampleFormat, StreamFormat, TapError, TapId};
    struct Tap {
        id: TapId,
        root: PathBuf,
        installed: Arc<std::sync::atomic::AtomicBool>,
    }
    impl AudioTap for Tap {
        fn id(&self) -> &TapId {
            &self.id
        }
        fn format(&self) -> StreamFormat {
            StreamFormat::new(16000, 1, SampleFormat::F32)
        }
        fn format_is_authoritative(&self) -> bool {
            true
        }
        fn start(&mut self, _: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
            let spec = Artifact {
                name: "custom-root".into(),
                revision: "0".repeat(40),
                sha256: Sha256::digest(b"fixture model")
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                bytes: 13,
                license: "MIT".into(),
            };
            let result = install_from(
                &self.root,
                &spec,
                &Control::default(),
                |_| Ok(u64::MAX),
                || Ok(Box::new(std::io::Cursor::new(b"fixture model"))),
            );
            self.installed.store(result.is_ok(), Ordering::Release);
            Ok(self.format())
        }
        fn stop(&mut self) -> Result<(), TapError> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn custom_session_directory_and_installer_share_library_admission() {
        let root = tempfile::tempdir().unwrap();
        let installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tap = Box::new(Tap {
            id: TapId::system_default(),
            root: root.path().to_owned(),
            installed: installed.clone(),
        });
        crate::session::run(
            &root.path().join("custom-captures"),
            tap,
            None,
            crate::session::Transcription::Disabled,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(
            !installed.load(Ordering::Acquire),
            "installer ran during capture when the session directory had a custom name"
        );
    }
}

#[cfg(all(test, unix))]
mod alias_tests {
    use super::*;
    #[test]
    fn library_aliases_share_admission_without_rejecting_the_alias_itself() {
        let root = tempfile::tempdir().unwrap();
        let alias = root.path().join("library link");
        std::os::unix::fs::symlink(root.path(), &alias).unwrap();
        let setup = WorkLease::setup(root.path()).unwrap();
        assert!(WorkLease::capture(&alias.join("custom-sessions")).is_err());
        drop(setup);
        assert!(WorkLease::capture(&alias.join("custom-sessions")).is_ok());
    }
}

// Exercise dashboard ownership against the real installer using tiny synthetic
// catalog bytes, without downloading or requiring model weights in CI.
#[cfg(test)]
pub(crate) fn install_dashboard_fixture(
    root: &Path,
    control: &Control,
    source: Box<dyn Read + Send>,
    progress: &mut dyn FnMut(Progress),
) -> Result<PathBuf, String> {
    let artifact = Artifact {
        name: "fixture".into(),
        revision: "0".repeat(40),
        sha256: digest(Sha256::new_with_prefix(b"fixture model")),
        bytes: 13,
        license: "MIT".into(),
    };
    install_with_progress(
        root,
        &artifact,
        control,
        |_| Ok(u64::MAX),
        || Ok(source),
        progress,
    )
}
