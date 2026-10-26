//! Single-use capture admission for a backend already moved off the caller.
use super::Coordinator;
use crate::failover::processing::Descriptor;
use fotw_pipeline::{
    aec::Backend,
    aec_worker::{Config, Status, Worker},
    reference_resample::Config as ReferenceConfig,
};
use std::sync::{Arc, Mutex};

/// An explicitly selected, preloaded processor for one recording. Clones share
/// one admission; they cannot start overlapping inference on the same backend.
/// This does not select or load an acoustic backend on behalf of the caller.
#[derive(Clone)]
pub struct Processing {
    claim: Arc<Mutex<Option<Prepared>>>,
    status: Status,
}
pub(crate) struct Prepared {
    worker: Worker,
    config: Config,
    pub(crate) descriptor: Descriptor,
}
impl std::fmt::Debug for Processing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Processing(<single-use>)")
    }
}
impl Processing {
    /// Move a preloaded backend onto an exclusive worker before capture. No
    /// native call or destructor runs on a realtime callback. The supplied
    /// descriptor identifies the actual backend/version/profile for replay.
    pub fn new<B: Backend + Send + 'static>(
        backend: B,
        config: Config,
        descriptor: Descriptor,
    ) -> Result<Self, String> {
        descriptor.validate().map_err(|e| e.to_string())?;
        if config.block_samples < 160 {
            return Err("processing block must hold at least 160 samples".into());
        }
        let worker =
            Worker::new(backend, config, 0, 0).map_err(|_| "invalid AEC worker configuration")?;
        let status = worker.status();
        Ok(Self {
            claim: Arc::new(Mutex::new(Some(Prepared {
                worker,
                config,
                descriptor,
            }))),
            status,
        })
    }
    /// Metadata-only native lifetime, including a blocked native destructor.
    pub fn status(&self) -> Status {
        self.status.clone()
    }
    pub(crate) fn take(&self) -> Result<Prepared, String> {
        self.claim
            .lock()
            .map_err(|_| "processing admission poisoned")?
            .take()
            .ok_or_else(|| "audio processor was already admitted to a recording".into())
    }
}
impl Prepared {
    pub(crate) fn deadline_ms(&self) -> u64 {
        self.config.deadline_ms
    }
    pub(crate) fn bind(
        self,
        config: ReferenceConfig,
        channels: (u16, u16),
        first: u64,
    ) -> Result<Coordinator, String> {
        Coordinator::bind_prepared(self.worker, self.config, config, channels, first)
    }
}
