//! Lifetime ownership for one daemon/native shell per library directory.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// OS-held ownership. Keep the file open for the entire daemon/shell lifetime.
/// The lock file must never be unlinked during normal cleanup: replacing its
/// inode would allow another owner while the original lock remained held.
#[derive(Debug)]
pub struct InstanceOwner {
    _file: File,
}
impl Drop for InstanceOwner {
    fn drop(&mut self) {
        // CLOEXEC closes inherited descriptors at exec, but a concurrent fork
        // may still hold one when this owner drops. End our ownership explicitly
        // instead of extending it until that unrelated child reaches exec.
        let _ = self._file.unlock();
    }
}
/// Why a second daemon cannot claim this library.
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    /// Another process is starting or serving this library.
    #[error(
        "another FlyOnTheWall process owns this library; reopen its dashboard or stop it before restarting"
    )]
    Busy,
    /// The lifetime lock could not safely be opened or acquired.
    #[error("could not claim the library's daemon ownership: {0}")]
    Io(#[from] std::io::Error),
}
impl InstanceOwner {
    /// Claim the canonical library directory, independently of the chosen port.
    ///
    /// # Errors
    /// Another owner, inaccessible storage or an unsafe existing lock file.
    pub fn acquire(sessions: &Path) -> Result<Self, InstanceError> {
        let path = lock_path(sessions)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("ownership lock is not a regular file").into());
        }
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(InstanceError::Busy),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}
fn lock_path(sessions: &Path) -> Result<PathBuf, std::io::Error> {
    let data_root = sessions.parent().unwrap_or(sessions);
    let data_root = if data_root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        data_root
    };
    std::fs::create_dir_all(data_root)?;
    Ok(data_root.canonicalize()?.join(".fotw-daemon.lock"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_excludes_another_handle_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let first = InstanceOwner::acquire(&sessions).unwrap();
        assert!(matches!(
            InstanceOwner::acquire(&sessions),
            Err(InstanceError::Busy)
        ));
        drop(first);
        assert!(InstanceOwner::acquire(&sessions).is_ok());
        assert!(root.path().join(".fotw-daemon.lock").exists());
    }
    #[cfg(unix)]
    #[test]
    fn dropping_owner_releases_ownership_while_a_fork_is_waiting_to_exec() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let owner = InstanceOwner::acquire(&sessions).unwrap();
        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Only async-signal-safe calls after fork in this threaded test.
            unsafe {
                libc::close(pipe[1]);
                let mut byte = 0_u8;
                libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
        }
        struct Fork(libc::pid_t, libc::c_int);
        impl Drop for Fork {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                    libc::close(self.1);
                }
            }
        }
        unsafe {
            libc::close(pipe[0]);
        }
        let _child = Fork(pid, pipe[1]);
        drop(owner);
        assert!(
            InstanceOwner::acquire(&sessions).is_ok(),
            "an inherited descriptor must not extend the dropped owner's lock"
        );
    }
    #[test]
    fn different_session_names_in_one_library_still_have_one_owner() {
        let root = tempfile::tempdir().unwrap();
        let _first = InstanceOwner::acquire(&root.path().join("sessions")).unwrap();
        assert!(matches!(
            InstanceOwner::acquire(&root.path().join("other-sessions")),
            Err(InstanceError::Busy)
        ));
    }
    #[test]
    fn independent_libraries_do_not_block_each_other() {
        let root = tempfile::tempdir().unwrap();
        let _first = InstanceOwner::acquire(&root.path().join("a/sessions")).unwrap();
        let _second = InstanceOwner::acquire(&root.path().join("b/sessions")).unwrap();
    }
    #[test]
    fn relative_sessions_use_the_current_library_directory() {
        assert_eq!(
            lock_path(Path::new("sessions")).unwrap(),
            std::env::current_dir()
                .unwrap()
                .canonicalize()
                .unwrap()
                .join(".fotw-daemon.lock")
        );
    }

    // Executed in an owned subprocess by the following test. A normal test run
    // has no role marker and performs no work here.
    #[test]
    fn ownership_child() {
        use std::io::Read;
        if std::env::var("FOTW_INSTANCE_TEST_ROLE").as_deref() != Ok("owned-child") {
            return;
        }
        let root =
            PathBuf::from(std::env::var_os("FOTW_INSTANCE_TEST_ROOT").expect("owned test root"));
        let _owner = InstanceOwner::acquire(&root.join("sessions")).unwrap();
        std::fs::write(root.join("child-ready"), b"ready").unwrap();
        let mut byte = [0_u8; 1];
        std::io::stdin().read_exact(&mut byte).unwrap();
    }

    #[test]
    fn a_live_process_excludes_another_and_exit_or_kill_releases_ownership() {
        use std::io::Write;
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};
        struct OwnedChild(Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for kill in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut child = OwnedChild(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "instance::tests::ownership_child"])
                    .env("FOTW_INSTANCE_TEST_ROLE", "owned-child")
                    .env("FOTW_INSTANCE_TEST_ROOT", root.path())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let began = Instant::now();
            while !root.path().join("child-ready").exists() {
                assert!(
                    began.elapsed() < Duration::from_secs(5),
                    "owned child did not claim its lock"
                );
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "owned child exited before claiming its lock"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(matches!(
                InstanceOwner::acquire(&root.path().join("sessions")),
                Err(InstanceError::Busy)
            ));
            if kill {
                child.0.kill().unwrap();
            } else {
                child.0.stdin.as_mut().unwrap().write_all(b"x").unwrap();
            }
            let status = child.0.wait().unwrap();
            if !kill {
                assert!(status.success());
            }
            assert!(InstanceOwner::acquire(&root.path().join("sessions")).is_ok());
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonical_directory_aliases_share_ownership() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let alias = root.path().join("alias");
        std::fs::create_dir(&data).unwrap();
        std::os::unix::fs::symlink(&data, &alias).unwrap();
        let _first = InstanceOwner::acquire(&data.join("sessions")).unwrap();
        assert!(matches!(
            InstanceOwner::acquire(&alias.join("sessions")),
            Err(InstanceError::Busy)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn a_lock_file_symlink_is_refused_without_modifying_its_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, b"owned fixture").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join(".fotw-daemon.lock")).unwrap();
        assert!(matches!(
            InstanceOwner::acquire(&root.path().join("sessions")),
            Err(InstanceError::Io(_))
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"owned fixture");
    }
}
