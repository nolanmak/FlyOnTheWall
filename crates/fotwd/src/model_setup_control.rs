//! Owned dashboard provisioning; installation runs outside HTTP and DB locks.
use crate::model_setup::{self, Control, Progress, Source};
use fotw_web::model_setup::{Job, Model, ModelSetupControl, Phase, Request, Status};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
type Installer = dyn Fn(&Path, &str, Source, &Control, &mut dyn FnMut(Progress)) -> Result<PathBuf, String>
    + Send
    + Sync;
/// No database handle, model inference or credential access.
pub struct DashboardModels {
    root: PathBuf,
    run: Arc<Installer>,
    job: Mutex<Option<Arc<OwnedJob>>>,
}
struct OwnedJob {
    status: Mutex<Job>,
    control: Control,
}
impl OwnedJob {
    fn report(&self, progress: Progress) {
        let mut status = lock(&self.status);
        if status.phase != Phase::Stopping {
            status.phase = match progress.phase {
                model_setup::Phase::Checking => Phase::Checking,
                model_setup::Phase::Transferring => Phase::Transferring,
                model_setup::Phase::Verifying => Phase::Verifying,
                model_setup::Phase::Installing => Phase::Installing,
                model_setup::Phase::Verified => Phase::Verified,
            };
        }
        status.bytes = progress.bytes;
        status.total = progress.total;
    }
    fn finish(&self, result: Result<PathBuf, String>) {
        let mut status = lock(&self.status);
        status.worker_running = false;
        if status.phase == Phase::Stopping {
            status.phase = Phase::Cancelled;
            status.message = Some("Setup cancelled. You can retry; previous weights were preserved before publication.".into());
        } else {
            match result {
                Ok(_) => {
                    status.phase = Phase::Verified;
                    status.message = Some("Model file verified. Choose it in Transcription and save; native readiness is checked before recording.".into());
                }
                Err(error) => {
                    status.phase = Phase::Failed;
                    status.message = Some(error);
                }
            }
        }
    }
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
impl std::fmt::Debug for DashboardModels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DashboardModels(<redacted>)")
    }
}
impl DashboardModels {
    /// Reading the catalog does not create directories or load a model.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            run: Arc::new(|root, name, source, control, progress| {
                model_setup::install(root, name, source, control, progress)
            }),
            job: Mutex::new(None),
        }
    }
}
impl Drop for DashboardModels {
    fn drop(&mut self) {
        if let Some(job) = lock(&self.job).as_ref() {
            let mut status = lock(&job.status);
            if status.worker_running && job.control.cancel() {
                status.phase = Phase::Stopping;
                status.message =
                    Some("Waiting for setup IO to finish before releasing this library.".into());
            }
        }
    }
}
impl ModelSetupControl for DashboardModels {
    fn read(&self) -> Status {
        let job = lock(&self.job)
            .as_ref()
            .map(|job| lock(&job.status).clone())
            .unwrap_or_default();
        // The speech detector is listed so its license and pin are visible;
        // installing any Whisper model installs it too.
        // The helper's models first, Parakeet Unified (the default on-device
        // model, #244) leading: each a set of pinned files in its store.
        let helper = crate::speechd::HELPER_MODELS.iter().map(|model| Model {
            name: model.id.into(),
            description: model.note.into(),
            bytes: model.bytes,
            sha256: "every file pinned by size and SHA-256 in fotw-speechd".into(),
            revision: model.revision.into(),
            license: model.license.into(),
            license_url: model.license_url(),
            present: crate::speechd::present_model(&self.root, model),
        });
        let models = helper
            .chain(
                crate::stt::KNOWN_MODELS
                    .iter()
                    .chain(std::iter::once(&crate::stt::VAD_MODEL))
                    .map(|model| Model {
                        name: model.name.into(),
                        description: model.note.into(),
                        bytes: model.bytes,
                        sha256: model.sha256.into(),
                        revision: model.revision.into(),
                        license: model.license.into(),
                        license_url: model.license_url(),
                        present: std::fs::symlink_metadata(
                            crate::stt::models_dir(&self.root)
                                .join(crate::stt::model_file_name(model.name)),
                        )
                        .is_ok_and(|m| m.is_file()),
                    }),
            )
            .collect();
        Status { models, job }
    }
    fn start(&self, request: Request) -> Result<Status, String> {
        let (name, total) = if let Some(model) = crate::speechd::helper_model(request.model()) {
            (model.id, model.bytes)
        } else {
            let model = crate::stt::catalog_model(request.model())
                .ok_or("Choose a model from the catalog.")?;
            (model.name, model.bytes)
        };
        let source = match request {
            Request::Download { .. } => Source::Download,
            Request::Import { path, .. } => {
                let path = path.trim();
                if path.len() > 4096
                    || path.chars().any(char::is_control)
                    || !Path::new(path).is_absolute()
                {
                    return Err(
                        "Choose a full local model file path without control characters.".into(),
                    );
                }
                Source::Import(PathBuf::from(path))
            }
        };
        let mut slot = lock(&self.job);
        let previous = slot
            .as_ref()
            .map(|job| lock(&job.status).clone())
            .unwrap_or_default();
        if previous.worker_running {
            return Err(
                "Model setup still owns this job. Cancel or wait, then refresh before retrying."
                    .into(),
            );
        }
        let id = fotw_store::new_id();
        let job = Arc::new(OwnedJob {
            status: Mutex::new(Job {
                id,
                model: Some(name.into()),
                phase: Phase::Checking,
                bytes: 0,
                total,
                worker_running: true,
                message: None,
            }),
            control: Control::default(),
        });
        *slot = Some(job.clone());
        let worker = job.clone();
        let root = self.root.clone();
        let run = self.run.clone();
        let name = name.to_owned();
        let spawned = std::thread::Builder::new()
            .name("model-setup".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&root, &name, source, &worker.control, &mut |progress| worker.report(progress))
                }))
                .unwrap_or_else(|_| Err("Model setup ended unexpectedly. Refresh and verify the installed file before retrying.".into()));
                // Includes source destruction and lease release, even on unwind.
                worker.finish(result);
            });
        if spawned.is_err() {
            let mut s = lock(&job.status);
            s.worker_running = false;
            s.phase = Phase::Failed;
            s.message = Some("Could not start the setup worker. Refresh to retry.".into());
        }
        drop(slot);
        Ok(self.read())
    }
    fn cancel(&self, job_id: &str) -> Result<Status, String> {
        let slot = lock(&self.job);
        let job = slot.as_ref().ok_or("There is no setup job to cancel.")?;
        let mut status = lock(&job.status);
        if status.id != job_id || !status.worker_running {
            return Err("This setup job has changed or finished. Refresh its status.".into());
        }
        if status.phase != Phase::Stopping {
            if !job.control.cancel() {
                return Err(
                    "Verified model publication already started. Refresh to see its result.".into(),
                );
            }
            status.phase = Phase::Stopping;
            status.message = Some(
                "Waiting for setup IO to finish. Retry stays disabled until it returns.".into(),
            );
        }
        drop(status);
        drop(slot);
        Ok(self.read())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    fn wait(c: &DashboardModels, phase: Phase) -> Status {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let s = c.read();
            if s.job.phase == phase && !s.job.worker_running {
                return s;
            }
            assert!(std::time::Instant::now() < until, "{:?}", s);
            std::thread::yield_now();
        }
    }
    #[test]
    fn dashboard_cancellation_retains_ownership_and_stale_job_cannot_cancel_next_install() {
        let root = tempfile::tempdir().unwrap();
        let mut c = DashboardModels::new(root.path().to_owned());
        let (entered, began) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let gate = Mutex::new(gate);
        c.run = Arc::new(move |_, _, _, control, progress| {
            entered.send(()).unwrap();
            gate.lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            progress(Progress {
                phase: model_setup::Phase::Transferring,
                bytes: 7,
                total: 13,
            });
            if control.cancel() {
                Ok(PathBuf::from("/fixture"))
            } else {
                Err("cancelled".into())
            }
        });
        let first = c
            .start(Request::Download {
                model: "small.en".into(),
            })
            .unwrap();
        began
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let stopped = c.cancel(&first.job.id).unwrap();
        assert_eq!(stopped.job.phase, Phase::Stopping);
        assert!(stopped.job.worker_running);
        assert!(
            c.start(Request::Download {
                model: "base.en".into()
            })
            .is_err()
        );
        release.send(()).unwrap();
        let terminal = wait(&c, Phase::Cancelled);
        assert!(!terminal.job.worker_running);
        let second = c
            .start(Request::Download {
                model: "base.en".into(),
            })
            .unwrap();
        began
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_ne!(first.job.id, second.job.id);
        assert!(c.cancel(&first.job.id).is_err());
        assert!(c.read().job.worker_running);
        release.send(()).unwrap();
        wait(&c, Phase::Verified);
    }
    #[test]
    fn dashboard_catalog_and_invalid_actions_never_open_sources_or_change_files() {
        let root = tempfile::tempdir().unwrap();
        let mut c = DashboardModels::new(root.path().to_owned());
        c.run = Arc::new(|_, _, _, _, _| panic!("source accessed"));
        let s = c.read();
        // The helper's models first (Parakeet Unified, then the live-caption
        // model, #245, and the diarizer, #246), then four Whisper models and
        // the speech detector, each with its license.
        assert_eq!(s.models.len(), 8);
        let parakeet = &s.models[0];
        assert_eq!(
            (
                parakeet.name.as_str(),
                parakeet.license.as_str(),
                parakeet.present
            ),
            ("parakeet-unified", "CC-BY-4.0", false)
        );
        assert!(
            parakeet
                .license_url
                .starts_with("https://huggingface.co/FluidInference/")
        );
        let captions = &s.models[1];
        assert_eq!(
            (
                captions.name.as_str(),
                captions.license.as_str(),
                captions.present
            ),
            (
                crate::captions::ENGINE,
                "LicenseRef-NVIDIA-Open-Model-License",
                false
            )
        );
        let diarizer = &s.models[2];
        assert_eq!(
            (diarizer.name.as_str(), diarizer.present),
            ("diarizer", false)
        );
        assert!(diarizer.license.contains("scoped-cc-by-4.0"));
        assert!(s.models[3..].iter().all(|m| m.license == "MIT"
            && m.revision.len() == 40
            && m.sha256.len() == 64
            && m.license_url.ends_with("/README.md")
            && !m.present));
        let detector = s.models.last().unwrap();
        assert_eq!(detector.name, crate::stt::VAD_MODEL.name);
        assert!(
            detector
                .license_url
                .starts_with("https://huggingface.co/ggml-org/whisper-vad/blob/")
        );
        assert!(
            c.start(Request::Download {
                model: "../bad".into()
            })
            .is_err()
        );
        assert!(
            c.start(Request::Import {
                model: "small.en".into(),
                path: "relative.bin".into()
            })
            .is_err()
        );
        assert!(!c.read().job.worker_running);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn dashboard_actual_installer_refuses_capture_and_late_cancel_cannot_revoke_publication() {
        let root = tempfile::tempdir().unwrap();
        let mut c = DashboardModels::new(root.path().to_owned());
        let (entered, began) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let gate = Mutex::new(gate);
        c.run = Arc::new(move |root, _, _, control, progress| {
            model_setup::install_dashboard_fixture(
                root,
                control,
                Box::new(std::io::Cursor::new(b"fixture model")),
                &mut |p| {
                    progress(p);
                    if p.phase == model_setup::Phase::Installing {
                        entered.send(()).unwrap();
                        gate.lock()
                            .unwrap()
                            .recv_timeout(std::time::Duration::from_secs(3))
                            .unwrap();
                    }
                },
            )
        });
        let job = c
            .start(Request::Import {
                model: "small.en".into(),
                path: "/fixture".into(),
            })
            .unwrap()
            .job;
        began
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(model_setup::WorkLease::capture(&root.path().join("sessions")).is_err());
        assert!(c.cancel(&job.id).is_err());
        assert!(c.read().job.worker_running);
        release.send(()).unwrap();
        let result = wait(&c, Phase::Verified);
        assert!(!result.job.worker_running);
        assert_eq!(
            std::fs::read(root.path().join("models/ggml-fixture.bin")).unwrap(),
            b"fixture model"
        );
        assert!(model_setup::WorkLease::capture(&root.path().join("sessions")).is_ok());
    }
    #[test]
    fn dashboard_shutdown_fences_blocked_io_and_retains_exclusion_through_source_drop() {
        struct Reader {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            dropping: mpsc::Sender<()>,
            finish: mpsc::Receiver<()>,
        }
        impl std::io::Read for Reader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
                buf[..13].copy_from_slice(b"fixture model");
                Ok(13)
            }
        }
        impl Drop for Reader {
            fn drop(&mut self) {
                let _ = self.dropping.send(());
                let _ = self.finish.recv_timeout(std::time::Duration::from_secs(3));
            }
        }
        let root = tempfile::tempdir().unwrap();
        let mut c = DashboardModels::new(root.path().to_owned());
        let (entered, began) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (dropping, dropped) = mpsc::channel();
        let (finish, end) = mpsc::channel();
        let source = Mutex::new(Some(Reader {
            entered,
            release: gate,
            dropping,
            finish: end,
        }));
        c.run = Arc::new(move |root, _, _, control, progress| {
            model_setup::install_dashboard_fixture(
                root,
                control,
                Box::new(source.lock().unwrap().take().unwrap()),
                progress,
            )
        });
        c.start(Request::Download {
            model: "small.en".into(),
        })
        .unwrap();
        began
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let job = lock(&c.job).as_ref().unwrap().clone();
        drop(c);
        assert_eq!(lock(&job.status).phase, Phase::Stopping);
        assert!(model_setup::WorkLease::capture(&root.path().join("sessions")).is_err());
        release.send(()).unwrap();
        dropped
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(lock(&job.status).worker_running);
        assert!(model_setup::WorkLease::capture(&root.path().join("sessions")).is_err());
        finish.send(()).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while lock(&job.status).worker_running {
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(lock(&job.status).phase, Phase::Cancelled);
        assert!(!root.path().join("models/ggml-fixture.bin").exists());
        assert!(model_setup::WorkLease::capture(&root.path().join("sessions")).is_ok());
    }
    #[test]
    fn dashboard_real_import_never_opens_source_during_capture() {
        let root = tempfile::tempdir().unwrap();
        let c = DashboardModels::new(root.path().to_owned());
        let capture = model_setup::WorkLease::capture(&root.path().join("sessions")).unwrap();
        c.start(Request::Import {
            model: "small.en".into(),
            path: root
                .path()
                .join("missing.bin")
                .to_string_lossy()
                .into_owned(),
        })
        .unwrap();
        let failed = wait(&c, Phase::Failed);
        assert!(failed.job.message.unwrap().contains("Capture"));
        assert!(!root.path().join("models").exists());
        drop(capture);
    }
    #[test]
    fn dashboard_worker_panic_is_terminal_and_does_not_leave_a_permanent_busy_job() {
        let root = tempfile::tempdir().unwrap();
        let mut c = DashboardModels::new(root.path().to_owned());
        c.run = Arc::new(|_, _, _, _, _| panic!("owned setup fixture panic"));
        c.start(Request::Download {
            model: "small.en".into(),
        })
        .unwrap();
        let failed = wait(&c, Phase::Failed);
        assert!(!failed.job.worker_running);
        assert!(failed.job.message.unwrap().contains("verify"));
        c.run = Arc::new(|_, _, _, _, _| Err("fixture source unavailable".into()));
        c.start(Request::Import {
            model: "small.en".into(),
            path: "/fixture".into(),
        })
        .unwrap();
        assert!(!wait(&c, Phase::Failed).job.worker_running);
    }
}
