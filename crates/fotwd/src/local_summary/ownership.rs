//! Cross-process admission. The stable lock file is never unlinked or replaced.
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

pub(super) struct OwnerLease(File, AtomicBool);

impl OwnerLease {
    /// Acquire before verification or native work. This directory is supplied
    /// by the app owner, shared across models, and separate from the cache.
    pub(super) fn acquire(directory: &Path) -> Result<Self, String> {
        let uid = unsafe { libc::geteuid() };
        let meta = std::fs::symlink_metadata(directory)
            .map_err(|_| "local runtime ownership directory unavailable")?;
        if !directory.is_absolute()
            || !meta.is_dir()
            || meta.uid() != uid
            || meta.mode() & 0o777 != 0o700
        {
            return Err("local runtime ownership directory must be private and owned".into());
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join("runtime.lock"))
            .map_err(|_| "local runtime ownership file unavailable")?;
        let meta = file
            .metadata()
            .map_err(|_| "local runtime ownership metadata unavailable")?;
        if !meta.is_file() || meta.uid() != uid || meta.nlink() != 1 || meta.mode() & 0o777 != 0o600
        {
            return Err("local runtime ownership file must be private and regular".into());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            return Err(if error.kind() == std::io::ErrorKind::WouldBlock {
                "local runtime busy; owned resources have not been released"
            } else {
                "local runtime ownership unavailable"
            }
            .into());
        }
        Ok(Self(file, AtomicBool::new(false)))
    }

    pub(super) fn file(&self) -> &File {
        &self.0
    }

    /// Before spawning the native ownership chain, stop automatically unlocking
    /// on Drop. Its inherited holders must now determine final release.
    pub(super) fn retain_for_native(&self) {
        self.1.store(true, Ordering::Release);
    }

    /// Release after native exit AND guardian reaping, or before any successful
    /// native spawn. Also releases copies in unrelated forks awaiting exec.
    pub(super) fn release(&self) -> Result<(), String> {
        if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err("local runtime ownership release unconfirmed".into());
        }
        Ok(())
    }
}

impl Drop for OwnerLease {
    fn drop(&mut self) {
        if !self.1.load(Ordering::Acquire) {
            // No native holder was spawned: release failed/cancelled file work
            // even if an unrelated concurrent fork has not reached exec yet.
            let _ = self.release();
        }
        // After native spawn, no implicit unlock: cancellation, crash or an
        // unconfirmed shutdown must retain admission in inherited holders.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        os::unix::{fs::PermissionsExt, process::CommandExt},
        process::{Command, Stdio},
    };

    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn lease_survives_the_original_owners_drop_until_the_exec_holder_exits() {
        let directory = private_directory();
        let lease = OwnerLease::acquire(directory.path()).unwrap();
        lease.retain_for_native();
        let fd = lease.file().as_raw_fd();
        let mut command = Command::new("/usr/bin/python3");
        command.args(["-IS", "-c", "import os,sys,threading; os.fstat(int(sys.argv[1])); threading.Timer(5,lambda:os._exit(0)).start(); print('ready',flush=True); sys.stdin.read(); os._exit(0)"])
            .arg(fd.to_string()).stdin(Stdio::piped()).stdout(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        drop(lease);
        let held_by_child = OwnerLease::acquire(directory.path()).err();
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert_eq!(ready.trim(), "ready");
        assert!(status.success());
        assert!(
            held_by_child.is_some_and(|error| error.contains("busy")),
            "dropping the original owner must not unlock a surviving native holder"
        );
        OwnerLease::acquire(directory.path())
            .unwrap()
            .release()
            .unwrap();
    }

    #[test]
    fn independent_app_owners_do_not_block_each_other_and_lock_files_persist() {
        let first = private_directory();
        let second = private_directory();
        let lease = OwnerLease::acquire(first.path()).unwrap();
        assert!(
            OwnerLease::acquire(first.path())
                .err()
                .unwrap()
                .contains("busy")
        );
        let other = OwnerLease::acquire(second.path()).unwrap();
        let inode = std::fs::metadata(first.path().join("runtime.lock"))
            .unwrap()
            .ino();
        lease.release().unwrap();
        // An old adapter may still exist after confirmed shutdown.
        let next = OwnerLease::acquire(first.path()).unwrap();
        drop(lease);
        assert!(OwnerLease::acquire(first.path()).is_err());
        assert_eq!(
            std::fs::metadata(first.path().join("runtime.lock"))
                .unwrap()
                .ino(),
            inode
        );
        next.release().unwrap();
        other.release().unwrap();
    }

    #[test]
    fn refuses_nonprivate_roots_and_linked_or_nonregular_lock_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(OwnerLease::acquire(directory.path()).is_err());
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.path().join("target");
        let lock = directory.path().join("runtime.lock");
        File::create(&target).unwrap();
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        assert!(OwnerLease::acquire(directory.path()).is_err());
        std::fs::remove_file(&lock).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&target, &lock).unwrap();
        assert!(OwnerLease::acquire(directory.path()).is_err());
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        assert!(OwnerLease::acquire(directory.path()).is_err());
        std::fs::remove_dir(&lock).unwrap();
        File::create(&lock).unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(OwnerLease::acquire(directory.path()).is_err());
    }
}
