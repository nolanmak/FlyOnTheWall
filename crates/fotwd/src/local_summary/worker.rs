//! Private same-binary workers. No library, provider or recording initialization.
use super::watch::ExitWatch;
use std::{
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

pub(super) const GUARDIAN: &str = "--fotw-summary-guardian";
const EXEC: &str = "--fotw-summary-exec";
pub(super) const READY: &str = "FOTW-SUMMARY/1 ";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Spec {
    pub version: u32,
    pub parent_pid: u32,
    pub worker: PathBuf,
    pub executable: PathBuf,
    pub directory: PathBuf,
    pub arguments: Vec<String>,
}

/// Dispatch private runtime workers before creating any application services.
///
/// A binary embedding LocalAdapter must call this before normal argument
/// handling. None means this is an ordinary app invocation. Worker errors
/// contain metadata only; the workers never receive meeting text or keys.
pub fn run_worker_if_requested() -> Option<Result<(), String>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mode = args.first()?.to_str()?;
    if mode != GUARDIAN && mode != EXEC {
        return None;
    }
    let result = (|| {
        let path = args.get(1).ok_or("local worker configuration missing")?;
        if mode == GUARDIAN && args.len() == 2 {
            guardian(Path::new(path))
        } else if mode == EXEC && args.len() == 3 {
            let parent = args[2]
                .to_str()
                .and_then(|v| v.parse().ok())
                .ok_or("local worker parent invalid")?;
            exec(Path::new(path), parent)
        } else {
            Err("local worker protocol invalid".into())
        }
    })();
    Some(result)
}

fn spec(path: &Path) -> Result<Spec, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "local worker configuration unavailable")?;
    let metadata = file
        .metadata()
        .map_err(|_| "local worker metadata unavailable")?;
    if !metadata.is_file() || metadata.len() > 32 * 1024 {
        return Err("local worker configuration invalid".into());
    }
    let mut bytes = Vec::new();
    file.take(32 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "local worker configuration unavailable")?;
    if bytes.len() > 32 * 1024 {
        return Err("local worker configuration exceeds its limit".into());
    }
    let spec: Spec =
        serde_json::from_slice(&bytes).map_err(|_| "local worker configuration invalid")?;
    if spec.version != 1
        || spec.parent_pid == 0
        || !spec.directory.is_absolute()
        || !spec.executable.is_absolute()
        || !spec.worker.is_absolute()
    {
        return Err("local worker version or identity mismatch".into());
    }
    Ok(spec)
}

fn die_group() -> ! {
    // guardian() checks that this is our own group leader before spawning
    // anything. Never use a PID or group supplied by a runtime response.
    unsafe {
        libc::kill(-libc::getpgrp(), libc::SIGKILL);
    }
    std::process::exit(1)
}
struct GroupCleanup;
impl Drop for GroupCleanup {
    fn drop(&mut self) {
        die_group();
    }
}

fn guardian(path: &Path) -> Result<(), String> {
    let own = std::process::id();
    if unsafe { libc::getpgrp() } as u32 != own {
        return Err("local guardian requires its own process group".into());
    }
    let spec = spec(path)?;
    if unsafe { libc::getppid() } as u32 != spec.parent_pid {
        return Err("local guardian parent exited before startup".into());
    }
    let parent = ExitWatch::new(spec.parent_pid)?;
    if unsafe { libc::getppid() } as u32 != spec.parent_pid {
        return Err("local guardian parent exited before startup".into());
    }
    // This wrapper cannot execute the native runtime until the app has its
    // PID and has installed an exit observer for it.
    let mut native = Command::new(&spec.worker)
        .args([std::ffi::OsStr::new(EXEC), path.as_os_str()])
        .arg(own.to_string())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", &spec.directory)
        .current_dir(&spec.directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "local native wrapper failed to start")?;
    let _cleanup = GroupCleanup;
    writeln!(std::io::stdout(), "{READY}{}", native.id())
        .and_then(|_| std::io::stdout().flush())
        .map_err(|_| "local worker handshake failed")?;
    let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err("local worker control unavailable".into());
    }
    loop {
        if parent.wait(Duration::ZERO)? {
            die_group();
        }
        let mut byte = [0_u8];
        match std::io::stdin().read(&mut byte) {
            Ok(1) if byte[0] == b'G' => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            _ => return Err("local worker cancelled before native exec".into()),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    native
        .stdin
        .take()
        .ok_or("local wrapper control missing")?
        .write_all(b"G")
        .map_err(|_| "local native exec control failed")?;
    loop {
        if parent.wait(Duration::from_secs(1))? {
            die_group();
        }
    }
}

fn exec(path: &Path, parent: u32) -> Result<(), String> {
    if unsafe { libc::getppid() } as u32 != parent || unsafe { libc::getpgrp() } as u32 != parent {
        return Err("local native wrapper has no owned guardian".into());
    }
    let spec = spec(path)?;
    let mut go = [0_u8];
    std::io::stdin()
        .read_exact(&mut go)
        .map_err(|_| "local native exec cancelled")?;
    if go[0] != b'G' || unsafe { libc::getppid() } as u32 != parent {
        return Err("local native exec cancelled".into());
    }
    let error = Command::new(&spec.executable)
        .args(&spec.arguments)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", &spec.directory)
        .current_dir(&spec.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .exec();
    let _ = error; // Never propagate a native message/path into logs.
    Err("local native exec failed".into())
}
