//! Real process ownership with synthetic files; no models or meeting libraries.
#![cfg(target_os = "macos")]

use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Parent(Option<Child>);
impl Parent {
    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Keep our direct child unreaped until signaling it, so its PID
            // cannot have been recycled for an unrelated process.
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}
impl Drop for Parent {
    fn drop(&mut self) {
        self.stop();
    }
}

struct ExitWatch {
    fd: OwnedFd,
    exited: bool,
}
impl ExitWatch {
    fn new(pid: u32) -> Self {
        let raw = unsafe { libc::kqueue() };
        assert!(raw >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
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
        assert_eq!(
            unsafe { libc::kevent(fd.as_raw_fd(), &change, 1, &mut receipt, 1, &zero) },
            1,
            "register the original owned PID before killing its parent"
        );
        let receipt_error = receipt.data;
        assert_eq!(
            receipt_error, 0,
            "native exited before observation was registered"
        );
        Self { fd, exited: false }
    }
    fn wait(&mut self, timeout: Duration) -> bool {
        if self.exited {
            return true;
        }
        let mut event = unsafe { std::mem::zeroed::<libc::kevent>() };
        let timeout = libc::timespec {
            tv_sec: timeout.as_secs().try_into().unwrap(),
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
        assert!(count >= 0, "owned exit observation failed");
        self.exited = count == 1 && event.fflags & libc::NOTE_EXIT != 0;
        self.exited
    }
}

fn artifact(path: &Path) -> serde_json::Value {
    let bytes = std::fs::read(path).unwrap();
    let digest: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    json!({"path":path,"sha256":digest})
}

#[test]
fn killing_the_app_parent_does_not_leave_its_native_model_running() {
    for mode in ["ready", "loading", "guardian_crash"] {
        let during_load = mode == "loading";
        let dir = tempfile::Builder::new()
            .prefix("fotw owner fixture-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let root = dir.path();
        let native_ready = root.join("native-ready");
        let app_ready = root.join("app-ready");
        let executable = root.join("llama-server");
        let denied_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let probe_port = denied_probe.local_addr().unwrap().port();
        // Isolated Python, tiny fake weights, and an unconditional ten-second
        // self-exit. The RED run waits for that exit before asserting failure;
        // it never signals a potentially recycled orphan PID.
        let code = format!(
            r#"#!/usr/bin/python3 -IS
import socket,sys,pathlib,threading,os,time,errno,json,fcntl
assert pathlib.Path(sys.argv[sys.argv.index('--model')+1]).read_bytes()==b'synthetic fixture weights'
assert pathlib.Path(sys.argv[sys.argv.index('--model')+1]).with_name('fixture-00002-of-00002.gguf').read_bytes()==b'synthetic second shard'
probe=None
try:
 probe=socket.socket(socket.AF_INET,socket.SOCK_STREAM)
 denied=probe.connect_ex(('127.0.0.1',{probe_port}))
except PermissionError as error:denied=error.errno
finally:
 if probe is not None:probe.close()
assert denied in (errno.EPERM,errno.EACCES)
# A lease descriptor must survive guardian -> wrapper -> native exec. A path
# lock held only by the app would not protect admission after an app crash.
lease_stat=os.stat({lease_path})
inherited=[]
for fd in range(3,256):
 try:
  info=os.fstat(fd)
  if (info.st_dev,info.st_ino)==(lease_stat.st_dev,lease_stat.st_ino):inherited.append(fd)
 except OSError:pass
assert len(inherited)==1
assert not (fcntl.fcntl(inherited[0],fcntl.F_GETFD)&fcntl.FD_CLOEXEC)
timer=threading.Timer(10,lambda:os._exit(0));timer.daemon=True;timer.start()
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
s.bind(sys.argv[sys.argv.index('--host')+1]);s.listen(2)
ready=pathlib.Path({ready})
partial=ready.with_suffix('.partial')
partial.write_text(json.dumps({{'pid':os.getpid(),'guardian':os.getppid(),'directory':str(pathlib.Path(sys.argv[sys.argv.index('--host')+1]).parent)}}))
partial.replace(ready)
while True:
 c,_=s.accept()
 data=b''
 while b'\r\n\r\n' not in data:
  part=c.recv(4096)
  if not part:break
  data+=part
 time.sleep({health_delay})
 answer=b'{{"status":"ok"}}'
 try:c.sendall(b'HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: '+str(len(answer)).encode()+b'\r\n\r\n'+answer)
 except BrokenPipeError:pass
 c.close()
"#,
            ready = serde_json::to_string(&native_ready).unwrap(),
            lease_path = serde_json::to_string(&root.join("summary-owner/runtime.lock")).unwrap(),
            health_delay = if during_load { 2 } else { 0 },
        );
        std::fs::write(&executable, code).unwrap();
        let model = root.join("fixture-00001-of-00002.gguf");
        std::fs::write(&model, b"synthetic fixture weights").unwrap();
        let second = root.join("fixture-00002-of-00002.gguf");
        std::fs::write(&second, b"synthetic second shard").unwrap();
        let owner = root.join("summary-owner");
        std::fs::create_dir(&owner).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&owner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({
                "ownership_directory":owner,"model_id":"fixture","runtime_files":[artifact(&executable)],
                "executable":"llama-server","model":artifact(&model),"model_shards":[artifact(&second)],
                "context_tokens":4096,"output_tokens":256,"threads":1,
                "load_timeout_seconds":10,"call_timeout_seconds":5
            }))
            .unwrap(),
        )
        .unwrap();
        let crash = root.join("crash");
        let followup = root.join("followup");
        let mut command = Command::new(env!("CARGO_BIN_EXE_fotwd"));
        command
            .arg("--test-local-summary-parent")
            .arg(&config)
            .arg(&app_ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if mode == "guardian_crash" {
            command.arg("--crash-owner").arg(&crash).arg(&followup);
        }
        let mut parent = Parent(Some(command.spawn().unwrap()));
        let app_pid = parent.0.as_ref().unwrap().id();
        let deadline = Instant::now() + Duration::from_secs(15);
        let ready = if during_load {
            &native_ready
        } else {
            &app_ready
        };
        while !ready.exists() {
            assert!(
                parent.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                "fixture app exited before native readiness"
            );
            assert!(
                Instant::now() < deadline,
                "fixture app did not become ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        if during_load {
            assert!(
                !app_ready.exists(),
                "this case must kill the app during native loading"
            );
        }
        let observed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(native_ready).unwrap()).unwrap();
        let pid = observed["pid"].as_u64().unwrap().try_into().unwrap();
        let guardian_pid = observed["guardian"].as_u64().unwrap().try_into().unwrap();
        // Admission must be shared by every app instance, not just requests
        // through one adapter. Never keep the probe's lock when it succeeds.
        use std::os::unix::fs::OpenOptionsExt;
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(owner.join("runtime.lock"))
            .unwrap();
        let available_while_running =
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if available_while_running {
            assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_UN) }, 0);
        }
        let mut native = ExitWatch::new(pid);
        let mut guardian = ExitWatch::new(guardian_pid);
        if mode == "guardian_crash" {
            std::fs::write(&crash, "crash only the retained guardian").unwrap();
        } else {
            parent.stop();
        }
        let promptly = native.wait(Duration::from_secs(1));
        let guardian_promptly = guardian.wait(Duration::from_secs(1));
        // Always clean up by observing the fake's own bounded exit, including RED.
        let finally = native.wait(Duration::from_secs(15));
        let guardian_finally = guardian.wait(Duration::from_secs(15));
        assert!(finally, "bounded fake native did not finish");
        assert!(guardian_finally, "owned guardian did not finish");
        if mode == "guardian_crash" {
            // No completion/health call was allowed to trigger cleanup before
            // this observation. Now verify that a later call is refused and
            // that the app can confirm shutdown normally.
            std::fs::write(&followup, "permit the refused followup").unwrap();
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = parent.0.as_mut().unwrap().try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "fixture app did not confirm stopped ownership"
                    );
                    parent.0.take();
                    break;
                }
                assert!(
                    Instant::now() < until,
                    "fixture app did not finish shutdown"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // SIGKILL bypassed the app's TempDir destructor. Remove only this
        // fixture's proven runtime staging, after both original processes exit.
        let directory = Path::new(observed["directory"].as_str().unwrap());
        assert_eq!(directory.parent(), Some(Path::new("/private/tmp")));
        assert!(
            directory
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("fotw-summary-")
        );
        if directory.exists() {
            assert!(std::fs::symlink_metadata(directory).unwrap().is_dir());
            let spec: serde_json::Value =
                serde_json::from_slice(&std::fs::read(directory.join("worker.json")).unwrap())
                    .unwrap();
            assert_eq!(spec["parent_pid"].as_u64(), Some(u64::from(app_pid)));
            std::fs::remove_dir_all(directory).unwrap();
        }
        assert!(
            !available_while_running,
            "a second app owner could acquire admission while a model was running"
        );
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "admission remained stuck after confirmed native and owner exit"
        );
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert!(
            promptly,
            "{mode}: an owner died but its owned native model remained running"
        );
        assert!(guardian_promptly, "the owned guardian outlived its app");
    }
}
