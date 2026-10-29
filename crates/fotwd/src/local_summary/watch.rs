//! Kernel observations of original process lifetimes, without reaping a child
//! or polling a numeric PID that another process could later reuse.
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub(super) struct ExitWatch {
    fd: OwnedFd,
    exited: AtomicBool,
}
impl ExitWatch {
    /// Register while the owner still retains the original child/PID.
    pub(super) fn new(pid: u32) -> Result<Self, String> {
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err("local process observation unavailable".into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err("local process observation unavailable".into());
        }
        let change = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT | libc::EV_RECEIPT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let mut receipt = unsafe { std::mem::zeroed::<libc::kevent>() };
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let count = unsafe { libc::kevent(fd.as_raw_fd(), &change, 1, &mut receipt, 1, &zero) };
        if count != 1 || receipt.flags & libc::EV_ERROR == 0 || receipt.data != 0 {
            return Err("original local process is unavailable".into());
        }
        Ok(Self {
            fd,
            exited: AtomicBool::new(false),
        })
    }

    /// Observe an exit event for that process, including across exec.
    pub(super) fn wait(&self, timeout: Duration) -> Result<bool, String> {
        if self.exited.load(Ordering::Acquire) {
            return Ok(true);
        }
        let mut event = unsafe { std::mem::zeroed::<libc::kevent>() };
        let timeout = libc::timespec {
            tv_sec: timeout
                .as_secs()
                .try_into()
                .map_err(|_| "invalid process observation deadline")?,
            tv_nsec: timeout.subsec_nanos().into(),
        };
        let count = unsafe {
            libc::kevent(
                self.fd.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        if count < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                return Ok(false);
            }
            return Err("local process observation failed".into());
        }
        if count == 1 {
            if event.flags & libc::EV_ERROR != 0 || event.fflags & libc::NOTE_EXIT == 0 {
                return Err("local process observation failed".into());
            }
            self.exited.store(true, Ordering::Release);
        }
        Ok(self.exited.load(Ordering::Acquire))
    }
}
