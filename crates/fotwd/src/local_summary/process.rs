use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::process::{Child, Command};

use super::{
    ipc::{self, Endpoint},
    package::{BoundModel, bind_models_with_cancellation},
};

/// A regular artifact from an authenticated catalog. Digests bind bytes, not
/// publisher trust: callers must authenticate the catalog before using this.
#[derive(Clone, serde::Deserialize)]
pub struct Artifact {
    /// Absolute installed file path. Model files must not be symlinks.
    pub path: PathBuf,
    /// Lowercase SHA-256 from the trusted manifest.
    pub sha256: String,
}

/// Explicit runtime configuration; there is no model-size or RAM-tier ceiling.
#[derive(Clone, serde::Deserialize)]
pub struct RuntimeConfig {
    /// Catalog model identity used in provenance, never a provider model name.
    pub model_id: String,
    /// Main llama-server executable and all adjacent runtime dylibs. These are
    /// copied and verified into a private directory before any execution.
    pub runtime_files: Vec<Artifact>,
    /// The executable basename in `runtime_files`.
    pub executable: String,
    /// Installed, verified GGUF weights. Downloading is a separate opt-in step.
    pub model: Artifact,
    /// Remaining GGUF shards in the same pinned package, when split. Each
    /// artifact is verified and held open; filenames preserve native discovery.
    #[serde(default)]
    pub model_shards: Vec<Artifact>,
    /// Explicit context allocation, including prompt and output.
    pub context_tokens: usize,
    /// Maximum output tokens admitted for a call.
    pub output_tokens: usize,
    /// CPU threads; selected by resource admission, not inferred from RAM alone.
    pub threads: usize,
    /// Declared cold-load deadline for this configuration.
    pub load_timeout_seconds: u64,
    /// Declared per-call deadline, including rendering and tokenization.
    pub call_timeout_seconds: u64,
}

/// Cancellation request for one owned model load. Requesting cancellation is
/// nonblocking; the owner must retain and await the load future to confirm
/// verification/native cleanup before admitting a replacement.
#[derive(Clone, Default)]
pub struct LoadCancellation(Arc<AtomicBool>);
impl LoadCancellation {
    /// Request cancellation. This acknowledges intent, not compute release.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub(super) fn check(&self) -> Result<(), String> {
        if self.0.load(Ordering::Acquire) {
            Err("local model load cancelled".into())
        } else {
            Ok(())
        }
    }
}

struct CancelOnDrop {
    control: LoadCancellation,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.control.cancel();
        }
    }
}

pub(super) struct Process {
    child: Mutex<Child>,
    pid: u32,
    stopped: AtomicBool,
    // Kept alive until the owner is dropped, including after cancellation.
    _root: tempfile::TempDir,
    pub(super) socket: PathBuf,
    _models: Option<BoundModel>,
}

impl Process {
    pub(super) fn pid(&self) -> Result<u32, String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("local runtime stopped; reload required".into());
        }
        let mut child = self
            .child
            .lock()
            .map_err(|_| "local runtime ownership unavailable")?;
        match child.try_wait() {
            Ok(None) => Ok(self.pid),
            _ => Err("local runtime exited; reload required".into()),
        }
    }

    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // The process group was created by us at spawn; this never targets an
        // arbitrary user daemon. Descendants inherit the same network sandbox.
        // Retain the unreaped leader until wait(), preventing PID reuse.
        if let Ok(mut child) = self.child.lock()
            && child.id().is_some()
        {
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
            let _ = child.start_kill();
        }
    }

    pub(super) async fn shutdown(&self) -> Result<(), String> {
        self.stop();
        // No await with a std mutex guard: try_wait also reaps the owned leader.
        let until = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let exited = self
                .child
                .lock()
                .map_err(|_| "local runtime ownership unavailable")?
                .try_wait()
                .map_err(|_| "local runtime wait failed")?
                .is_some();
            if exited {
                return Ok(());
            }
            if tokio::time::Instant::now() >= until {
                return Err("local runtime exit not confirmed".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

const PROFILE: &str = "(version 1)(allow default)(deny network*)(allow network-inbound (local unix-socket (path-literal (param \"FOTW_SOCKET\"))))";

pub(super) fn validate(config: &RuntimeConfig) -> Result<(), String> {
    if config.model_id.is_empty()
        || config.model_id.len() > 256
        || config.context_tokens < 512
        || config.output_tokens == 0
        || config.output_tokens >= config.context_tokens
        || config.threads == 0
        || config.load_timeout_seconds == 0
        || config.call_timeout_seconds == 0
        || tokio::time::Instant::now()
            .checked_add(Duration::from_secs(config.load_timeout_seconds))
            .is_none()
        || tokio::time::Instant::now()
            .checked_add(Duration::from_secs(config.call_timeout_seconds))
            .is_none()
        || config.runtime_files.is_empty()
        || config.runtime_files.len() > 128
        || config.executable != "llama-server"
    {
        return Err("invalid local runtime configuration".into());
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn verified_copy(
    artifact: &Artifact,
    destination: Option<&Path>,
    max_bytes: Option<u64>,
) -> Result<File, String> {
    verified_copy_cancellable(
        artifact,
        destination,
        max_bytes,
        &LoadCancellation::default(),
    )
}

pub(super) fn verified_copy_cancellable(
    artifact: &Artifact,
    destination: Option<&Path>,
    max_bytes: Option<u64>,
    cancellation: &LoadCancellation,
) -> Result<File, String> {
    cancellation.check()?;
    if !artifact.path.is_absolute()
        || artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid local artifact identity".into());
    }
    use std::os::unix::fs::OpenOptionsExt;
    let mut input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&artifact.path)
        .map_err(|_| "local artifact unavailable")?;
    let metadata = input
        .metadata()
        .map_err(|_| "local artifact metadata unavailable")?;
    if !metadata.is_file() || metadata.len() == 0 || max_bytes.is_some_and(|m| metadata.len() > m) {
        return Err("invalid local artifact size/type".into());
    }
    let mut output = destination
        .map(|p| {
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(p)
        })
        .transpose()
        .map_err(|_| "private runtime staging failed")?;
    let digest = hash_chunks(&mut input, &mut output, metadata.len(), cancellation)?;
    if digest != artifact.sha256 {
        return Err("local artifact checksum mismatch".into());
    }
    if let Some(mut output) = output {
        output
            .flush()
            .map_err(|_| "private runtime staging failed")?;
    }
    input
        .seek(SeekFrom::Start(0))
        .map_err(|_| "local artifact seek failed")?;
    Ok(input)
}

fn hash_chunks(
    input: &mut dyn Read,
    output: &mut Option<File>,
    expected_length: u64,
    cancellation: &LoadCancellation,
) -> Result<String, String> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    let mut read_bytes = 0_u64;
    loop {
        cancellation.check()?;
        let n = input
            .read(&mut buffer)
            .map_err(|_| "local artifact read failed")?;
        if n == 0 {
            break;
        }
        read_bytes = read_bytes
            .checked_add(n as u64)
            .ok_or("local artifact size overflow")?;
        if read_bytes > expected_length {
            return Err("local artifact changed during verification".into());
        }
        hash.update(&buffer[..n]);
        if let Some(output) = output {
            output
                .write_all(&buffer[..n])
                .map_err(|_| "private runtime staging failed")?;
        }
    }
    cancellation.check()?;
    if read_bytes != expected_length {
        return Err("local artifact changed during verification".into());
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn prepare(
    config: &RuntimeConfig,
    cancellation: &LoadCancellation,
) -> Result<(tempfile::TempDir, BoundModel), String> {
    validate(config)?;
    // A short canonical socket path also avoids Darwin's 104-byte sun_path.
    let root = tempfile::Builder::new()
        .prefix("fotw-summary-")
        .tempdir_in("/private/tmp")
        .map_err(|_| "private runtime directory unavailable")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "private runtime permissions failed")?;
    let mut names = std::collections::HashSet::new();
    for artifact in &config.runtime_files {
        let name = artifact
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid runtime filename")?;
        if !names.insert(name.to_ascii_lowercase())
            || !(name == config.executable || name.ends_with(".dylib"))
        {
            return Err("invalid runtime file set".into());
        }
        let destination = root.path().join(name);
        verified_copy_cancellable(
            artifact,
            Some(&destination),
            Some(256 * 1024 * 1024),
            cancellation,
        )?;
        if name == config.executable {
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "private runtime executable permissions failed")?;
        }
    }
    if !names.contains(&config.executable) {
        return Err("runtime executable missing from manifest".into());
    }
    let model = bind_models_with_cancellation(
        &config.model,
        &config.model_shards,
        root.path(),
        cancellation,
    )?;
    Ok((root, model))
}

pub(super) async fn launch(
    config: &RuntimeConfig,
    cancellation: &LoadCancellation,
) -> Result<Process, String> {
    cancellation.check()?;
    validate(config)?;
    let mut guard = CancelOnDrop {
        control: cancellation.clone(),
        armed: true,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.load_timeout_seconds);
    let copy = config.clone();
    let control = cancellation.clone();
    let mut verification = tokio::task::spawn_blocking(move || prepare(&copy, &control));
    let (root, model) = match tokio::time::timeout_at(deadline, &mut verification).await {
        Ok(result) => result.map_err(|_| "local artifact verification failed")??,
        Err(_) => {
            cancellation.cancel();
            // Retain ownership until the blocking reader observes cancellation
            // and destroys its private staging files. A timeout is not release.
            let _ = verification.await;
            return Err("local model load timed out".into());
        }
    };
    cancellation.check()?;
    if tokio::time::Instant::now() >= deadline {
        return Err("local model load timed out".into());
    }
    let socket = root.path().join("server.sock");
    let mut command =
        sandboxed_command(&root.path().join(&config.executable), root.path(), &socket);
    command
        .args([
            "--offline",
            "--no-webui",
            "--no-agent",
            "--log-disable",
            "--no-context-shift",
            "--jinja",
        ])
        .arg("--host")
        .arg(&socket)
        .arg("--model")
        .arg(&model.first_path)
        .arg("--alias")
        .arg(&config.model_id)
        .arg("--ctx-size")
        .arg(config.context_tokens.to_string())
        .arg("--threads")
        .arg(config.threads.to_string())
        .args(["--parallel", "1", "--cache-ram", "0", "--no-warmup"]);
    for file in &model.files {
        inherit_model(&mut command, file);
    }
    let child = command
        .spawn()
        .map_err(|_| "network-isolated local runtime launch failed")?;
    let pid = child.id().ok_or("local runtime has no owned PID")?;
    let process = Process {
        child: Mutex::new(child),
        pid,
        stopped: AtomicBool::new(false),
        socket,
        _root: root,
        _models: Some(model),
    };
    loop {
        if let Err(error) = cancellation.check() {
            return finish_failed_load(process, error).await;
        }
        let pid = match process.pid() {
            Ok(pid) => pid,
            Err(error) => {
                return finish_failed_load(process, error).await;
            }
        };
        if let Ok(value) = ipc::request(
            &process.socket,
            pid,
            Endpoint::Health,
            None,
            Duration::from_millis(250),
        )
        .await
            && value["status"] == "ok"
        {
            if let Err(error) = cancellation.check() {
                return finish_failed_load(process, error).await;
            }
            guard.armed = false;
            return Ok(process);
        }
        if tokio::time::Instant::now() >= deadline {
            return finish_failed_load(process, "local model load timed out".into()).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn finish_failed_load(process: Process, error: String) -> Result<Process, String> {
    // An unconfirmed exit must keep this load's owner and staging resources
    // alive. The service acknowledges cancellation separately and retains this
    // future until cleanup actually finishes; it must not abort-and-replace.
    while process.shutdown().await.is_err() {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(error)
}

fn sandboxed_command(executable: &Path, root: &Path, socket: &Path) -> Command {
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .args([
            "-D",
            &format!("FOTW_SOCKET={}", socket.display()),
            "-p",
            PROFILE,
        ])
        .arg(executable)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", root)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    command
}

fn inherit_model(command: &mut Command, model: &File) {
    let fd = model.as_raw_fd();
    // Keep the verified inode through exec: a cache pathname replaced after
    // verification must not change the model actually opened by llama.cpp.
    // Only this read-only model descriptor loses CLOEXEC, in the child only.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
/// Own a deliberately slow fixture and publish listening readiness separately
/// from socket creation and request receipt.
pub(super) fn stalled_fixture(startup_delay: Duration) -> (Process, PathBuf, PathBuf) {
    let root = tempfile::Builder::new()
        .prefix("fotw-summary-test-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let socket = root.path().join("owned.sock");
    let marker = root.path().join("received");
    let listening = root.path().join("listening");
    let code = r#"import socket,pathlib
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.bind('owned.sock');time.sleep(0.2);s.listen(1);pathlib.Path('listening').write_text('ready')
c,_=s.accept();c.recv(8192);pathlib.Path('received').write_text('received')
while c.recv(8192):pass
"#;
    let code = format!(
        "import time;time.sleep({});\n{code}",
        startup_delay.as_secs_f64()
    );
    let child = sandboxed_command(Path::new("/usr/bin/python3"), root.path(), &socket)
        .args(["-I", "-S", "-c", &code])
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    (
        Process {
            child: Mutex::new(child),
            pid,
            stopped: AtomicBool::new(false),
            _root: root,
            _models: None,
            socket,
        },
        listening,
        marker,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_load_never_opens_missing_artifacts_or_spawns_a_child() {
        let root = tempfile::tempdir().unwrap();
        let missing = Artifact {
            path: root.path().join("model.gguf"),
            sha256: "0".repeat(64),
        };
        let runtime = Artifact {
            path: root.path().join("llama-server"),
            sha256: "0".repeat(64),
        };
        let config = RuntimeConfig {
            model_id: "cancelled fixture".into(),
            runtime_files: vec![runtime],
            executable: "llama-server".into(),
            model: missing,
            model_shards: vec![],
            context_tokens: 8192,
            output_tokens: 512,
            threads: 1,
            load_timeout_seconds: 1,
            call_timeout_seconds: 1,
        };
        let cancellation = LoadCancellation::default();
        cancellation.cancel();
        let error = launch(&config, &cancellation).await.err().unwrap();
        assert!(error.contains("cancelled"), "{error}");
    }

    #[test]
    fn cancelled_verification_stops_before_another_chunk_read() {
        struct CancelOnRead {
            control: LoadCancellation,
            reads: usize,
        }
        impl Read for CancelOnRead {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                assert_eq!(
                    self.reads, 0,
                    "cancelled verification must not read another chunk"
                );
                self.reads += 1;
                bytes[0] = 7;
                self.control.cancel();
                Ok(1)
            }
        }
        let cancellation = LoadCancellation::default();
        let mut input = CancelOnRead {
            control: cancellation.clone(),
            reads: 0,
        };
        let error = hash_chunks(&mut input, &mut None, 1, &cancellation)
            .err()
            .unwrap();
        assert!(error.contains("cancelled"), "{error}");
        assert_eq!(input.reads, 1);
    }

    #[tokio::test]
    async fn child_opens_verified_inode_even_if_cache_path_is_replaced() {
        let root = tempfile::Builder::new()
            .prefix("fotw-summary-test-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let path = root.path().join("model");
        std::fs::write(&path, b"verified").unwrap();
        let hash = Sha256::digest(b"verified")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let model = verified_copy(
            &Artifact {
                path: path.clone(),
                sha256: hash,
            },
            None,
            None,
        )
        .unwrap();
        std::fs::rename(&path, root.path().join("original")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        let mut command = sandboxed_command(
            Path::new("/usr/bin/python3"),
            root.path(),
            &root.path().join("owned.sock"),
        );
        inherit_model(&mut command, &model);
        let output = command
            .args([
                "-I",
                "-S",
                "-c",
                &format!(
                    "import pathlib;print(pathlib.Path('/dev/fd/{}').read_text())",
                    model.as_raw_fd()
                ),
            ])
            .stdout(std::process::Stdio::piped())
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"verified\n");
    }

    #[test]
    fn verifies_regular_bytes_before_execution_and_never_follows_model_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fixture");
        std::fs::write(&file, b"synthetic artifact").unwrap();
        let mut artifact = Artifact {
            path: file.clone(),
            sha256: Sha256::digest(b"synthetic artifact")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        };
        assert!(verified_copy(&artifact, None, None).is_ok());
        artifact.sha256 = "0".repeat(64);
        assert!(
            verified_copy(&artifact, None, None)
                .unwrap_err()
                .contains("checksum")
        );
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(file, &link).unwrap();
        artifact.path = link;
        assert!(verified_copy(&artifact, None, None).is_err());
    }

    // These are attack fixtures, not legitimate generation: connection
    // attempts are made only to listeners this test owns, never the Internet.
    #[tokio::test]
    async fn sandbox_denies_tcp_udp_unix_relays_and_descendants_inherit_it() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let root = tempfile::Builder::new()
            .prefix("fotw-summary-test-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let relay = root.path().join("relay.sock");
        let _unix = std::os::unix::net::UnixListener::bind(&relay).unwrap();
        let probe = format!(
            r#"import socket,json,errno
results=[]
for kind,address in [(socket.SOCK_STREAM,('127.0.0.1',{})),(socket.SOCK_DGRAM,('127.0.0.1',{})),(socket.SOCK_STREAM,{:?})]:
 s=socket.socket(socket.AF_UNIX if isinstance(address,str) else socket.AF_INET,kind);s.settimeout(1)
 try:
  s.connect(address)
  if kind==socket.SOCK_DGRAM:s.send(b'fixture')
  results.append('allowed')
 except OSError as e:results.append('denied' if e.errno in (errno.EPERM,errno.EACCES) else 'unexpected')
 finally:s.close()
print(json.dumps(results))
"#,
            tcp.local_addr().unwrap().port(),
            udp.local_addr().unwrap().port(),
            relay.to_str().unwrap()
        );
        let baseline = Command::new("/usr/bin/python3")
            .args(["-I", "-S", "-c", &probe])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()
            .await
            .unwrap();
        assert!(baseline.status.success());
        assert_eq!(
            serde_json::from_slice::<Vec<String>>(&baseline.stdout).unwrap(),
            vec!["allowed"; 3]
        );
        let socket = root.path().join("owned.sock");
        for code in [
            probe.clone(),
            format!(
                "import subprocess,sys;subprocess.run([sys.executable,'-I','-S','-c',{probe:?}],check=True)"
            ),
        ] {
            let output = sandboxed_command(Path::new("/usr/bin/python3"), root.path(), &socket)
                .args(["-I", "-S", "-c", &code])
                .stdout(std::process::Stdio::piped())
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
            assert_eq!(
                serde_json::from_slice::<Vec<String>>(&output.stdout).unwrap(),
                vec!["denied"; 3]
            );
        }
    }

    #[tokio::test]
    async fn owns_only_its_child_and_confirms_exit_after_stop() {
        let root = tempfile::Builder::new()
            .prefix("fotw-summary-test-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let socket = root.path().join("owned.sock");
        let child = sandboxed_command(Path::new("/usr/bin/python3"), root.path(), &socket)
            .args(["-I", "-S", "-c", "import time;time.sleep(30)"])
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let process = Process {
            child: Mutex::new(child),
            pid,
            stopped: AtomicBool::new(false),
            _root: root,
            _models: None,
            socket,
        };
        assert_eq!(process.pid().unwrap(), pid);
        process.shutdown().await.unwrap();
        assert!(process.pid().is_err());
        assert!(process.child.lock().unwrap().try_wait().unwrap().is_some());
    }
}
