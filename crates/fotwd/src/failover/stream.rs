//! One durable cloud-to-local source. Disk IO and native inference run on a
//! dedicated worker; the capture pump only appends canonical audio. The session
//! API supports this runtime; app settings/startup recovery are separate wiring.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fotw_pipeline::replay::{MAX_READ_SAMPLES, ReplayReader, ReplaySource, ReplayWriter};
use fotw_stt::local::{Chunk, EndpointConfig, Endpointer, LocalStreamConfig, segments_for};
use fotw_stt::{
    DeepgramStream, DeepgramStreamConfig, Recognizer, RecognizerFactory, Source, StreamEvent,
    StreamState, SttCapabilities, SttError, SttErrorClass, TranscriptSegment,
};
use tokio::sync::{mpsc, watch};

use super::coverage::{
    CommitOutcome, CoverageJournal, EngineDescriptor, LocalPlan, ProviderEpoch, SwitchReason,
};
use super::policy::{Decision, HandoffLimits, HandoffPolicy};

/// Per-source input and shutdown settings. Contains a cloud credential; never
/// serialize or log this object.
pub struct FailoverConfig {
    /// Optional immutable audio-processing identity, declared before input.
    /// Such sources accept only provenance-bearing `write_processed` calls.
    pub processing: Option<super::processing::Descriptor>,
    /// Cloud transport, forced into provider-confirmed final mode on admission.
    pub cloud: DeepgramStreamConfig,
    /// Shared loaded model factory, with an independent state per source.
    pub local: Arc<dyn RecognizerFactory>,
    /// Local model revision/checksum, if known by preflight.
    pub local_revision: Option<String>,
    /// Conservative defaults; admission can supply capabilities validated from
    /// the selected model. Never copy cloud upload/diarization capabilities.
    pub local_capabilities: SttCapabilities,
    /// Deterministic failure policy budgets.
    pub limits: HandoffLimits,
    /// Local utterance endpointing.
    pub endpoint: EndpointConfig,
    /// Total queued and in-flight cloud samples.
    pub cloud_queue_samples: usize,
    /// Time allowed for a cloud Stop final before local finishes the tail.
    pub cloud_settle: Duration,
    /// Entire input-freeze and transcript-drain deadline.
    pub stop_timeout: Duration,
}

impl FailoverConfig {
    /// Defaults for one explicitly selected fallback source.
    #[must_use]
    pub fn new(cloud: DeepgramStreamConfig, local: Arc<dyn RecognizerFactory>) -> Self {
        Self {
            processing: None,
            cloud,
            local,
            local_revision: None,
            local_capabilities: SttCapabilities {
                streaming: true,
                batch: false,
                word_timestamps: fotw_stt::FeatureAvailability::None,
                diarization: fotw_stt::FeatureAvailability::None,
                language_detection: false,
                custom_vocabulary: fotw_stt::CustomVocabulary::None,
                max_file_bytes: 0,
                max_file_seconds: 0,
                native_rates: vec![16_000],
                retention_control: fotw_stt::RetentionControl::None,
                supports_replay_faster_than_realtime: true,
            },
            limits: HandoffLimits::default(),
            endpoint: EndpointConfig::default(),
            cloud_queue_samples: 32_000,
            cloud_settle: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(30),
        }
    }
}

/// Live on-device source configuration. It has no cloud key or transport.
pub struct LocalSourceConfig {
    /// Session/source clock and utterance policy.
    pub stream: LocalStreamConfig,
    /// Shared model factory; loading stays on the exclusive speech worker.
    pub local: Arc<dyn RecognizerFactory>,
    /// Pinned/expected model revision; this does not assert native readiness.
    pub revision: Option<String>,
    /// Optional declared audio-processing identity.
    pub processing: Option<super::processing::Descriptor>,
    /// Bounded inference drain; native destruction remains separately visible.
    pub stop_timeout: Duration,
}

/// Explicit cloud-only durable source. Contains a provider credential, never
/// serialize or log it. No local factory or backup model can be supplied.
pub struct CloudSourceConfig {
    /// Provider transport and canonical source identity/clock.
    pub cloud: DeepgramStreamConfig,
    /// Optional immutable audio processor declared before input acceptance.
    pub processing: Option<super::processing::Descriptor>,
    /// Maximum queued/in-flight canonical samples.
    pub cloud_queue_samples: usize,
    /// Time allowed for confirmed final coverage after Stop.
    pub cloud_settle: Duration,
    /// Bounded input-freeze and transcript-drain deadline.
    pub stop_timeout: Duration,
}
impl CloudSourceConfig {
    /// Configure durable cloud transcription without selecting local inference.
    #[must_use]
    pub fn new(cloud: DeepgramStreamConfig) -> Self {
        Self {
            cloud,
            processing: None,
            cloud_queue_samples: 32000,
            cloud_settle: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(30),
        }
    }
}
impl LocalSourceConfig {
    /// Configure a source without opening a model, socket or credential store.
    #[must_use]
    pub fn new(stream: LocalStreamConfig, local: Arc<dyn RecognizerFactory>) -> Self {
        Self {
            stream,
            local,
            revision: None,
            processing: None,
            stop_timeout: Duration::from_secs(30),
        }
    }
    fn plan(&self) -> LocalPlan {
        LocalPlan {
            engine: EngineDescriptor {
                provider: fotw_stt::local::PROVIDER.into(),
                model: self.local.model().into(),
                revision: self.revision.clone(),
                capabilities: local_capabilities(),
            },
            language: self.local.language().map(str::to_owned),
            endpoint: self.stream.endpoint,
        }
    }
}
fn local_capabilities() -> SttCapabilities {
    SttCapabilities {
        streaming: true,
        batch: false,
        word_timestamps: fotw_stt::FeatureAvailability::None,
        diarization: fotw_stt::FeatureAvailability::None,
        language_detection: false,
        custom_vocabulary: fotw_stt::CustomVocabulary::None,
        max_file_bytes: 0,
        max_file_seconds: 0,
        native_rates: vec![16000],
        retention_control: fotw_stt::RetentionControl::None,
        supports_replay_faster_than_realtime: true,
    }
}

impl FailoverConfig {
    fn local_plan(&self) -> LocalPlan {
        LocalPlan {
            engine: EngineDescriptor {
                provider: fotw_stt::local::PROVIDER.into(),
                model: self.local.model().into(),
                revision: self.local_revision.clone(),
                capabilities: self.local_capabilities.clone(),
            },
            language: self.local.language().map(str::to_owned),
            endpoint: self.endpoint,
        }
    }
}

/// Explicit local recovery request. It has no cloud key, endpoint, or policy.
/// The model revision and factory language must match the saved ready plan.
pub struct LocalResumeConfig {
    /// Original WAL identity.
    pub session_id: String,
    /// Source whose pending canonical audio is being recovered.
    pub source: Source,
    /// Installed local model, independently preflighted by the caller.
    pub local: Arc<dyn RecognizerFactory>,
    /// Verified installed revision/checksum, not the current settings preference.
    pub revision: Option<String>,
    /// Bounded inference drain; unresolved native ownership remains observable.
    pub stop_timeout: Duration,
}

struct WorkerConfig {
    session_id: String,
    source: Source,
    local: Option<LocalRuntime>,
    endpoint: EndpointConfig,
    limits: HandoffLimits,
    cloud_queue_samples: usize,
    cloud_settle: Duration,
    stop_timeout: Duration,
    prefix_end: Option<u64>,
}
struct LocalRuntime {
    factory: Arc<dyn RecognizerFactory>,
    plan: LocalPlan,
}

struct SourceInputs {
    native: Option<NativeLease>,
    processing: Option<super::processing::Journal>,
    writer: ReplayWriter,
    journal: CoverageJournal,
    reader: ReplayReader,
    recognizer: Option<Box<dyn Recognizer>>,
}

/// Handoff-aware event stream. Only this source's final Closed is terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Transcript/provider state on the common STT contract.
    Transcript(StreamEvent),
    /// Durably recorded provider change before any new-epoch transcript.
    ProviderSwitch(ProviderEpoch),
}

/// Current engine lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Cloud provider, with its retry budget active when needed.
    Cloud,
    /// Sticky on-device inference for this meeting.
    Local,
    /// All frozen input was durably accounted for.
    Complete,
    /// The requested saved prefix was accounted for; remaining original audio
    /// has no completion or retirement proof from this operation.
    PrefixRecovered,
    /// Audio retained; a pending interval needs retry.
    Incomplete,
}

/// Snapshot for recording status and recovery UI.
#[derive(Debug, Clone)]
pub struct Status {
    /// Engine lifecycle.
    pub phase: Phase,
    /// Current provider/model provenance and fencing token.
    pub epoch: ProviderEpoch,
    /// Absolute end of committed audio.
    pub committed_sample: u64,
    /// Absolute end accepted by the pump, or the explicit target for prefix
    /// recovery. Other original audio is outside a prefix operation's scope.
    pub written_sample: u64,
    /// Target audio still awaiting transcription or silence acknowledgement;
    /// zero lag on a prefix does not account for other retained original audio.
    pub lag_samples: u64,
    /// Worker/native cleanup is still outstanding.
    pub worker_running: bool,
    /// An incomplete-transcript explanation.
    pub error: Option<String>,
}

/// Read-only telemetry detached from stream, model and canonical-file ownership.
#[derive(Clone)]
pub struct StatusReader {
    gate: Arc<Mutex<Publication>>,
    end: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
}
impl std::fmt::Debug for StatusReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StatusReader(<redacted>)")
    }
}
impl StatusReader {
    /// Read current metadata and coverage without IO or native work.
    pub fn read(&self) -> Status {
        Self::snapshot(&self.gate, &self.end, &self.done)
    }
    fn snapshot(gate: &Mutex<Publication>, end: &AtomicU64, done: &AtomicBool) -> Status {
        let mut status = gate
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status
            .clone();
        status.written_sample = end.load(Ordering::Acquire);
        status.lag_samples = status
            .written_sample
            .saturating_sub(status.committed_sample);
        status.worker_running = !done.load(Ordering::Acquire);
        status
    }
}

struct Publication {
    fenced: bool,
    status: Status,
}
struct Writer {
    processing: Option<super::processing::Journal>,
    audio: ReplayWriter,
    last_sync: Instant,
}
impl Writer {
    fn sync(&mut self) -> std::io::Result<()> {
        if let Some(processing) = self.processing.as_mut() {
            processing.sync()?;
        }
        self.audio.sync()?;
        self.last_sync = Instant::now();
        Ok(())
    }
    fn seal(&mut self) -> std::io::Result<u64> {
        self.sync()?;
        self.audio.seal()
    }
}
struct Shared {
    writer: Mutex<Writer>,
    end: Arc<AtomicU64>,
    stop_requested: AtomicBool,
    input_closed: AtomicBool,
    done: Arc<AtomicBool>,
    gate: Arc<Mutex<Publication>>,
    wake: Condvar,
    wait: Mutex<()>,
    events: mpsc::UnboundedSender<Event>,
    prefix_only: bool,
}
struct NativeExit {
    done: Arc<AtomicBool>,
    notice: watch::Sender<bool>,
}
struct NativeLease(std::fs::File);
impl NativeLease {
    fn acquire(dir: &Path, source: Source) -> std::io::Result<Self> {
        let name = match source {
            Source::System => "system",
            Source::Mic => "mic",
        };
        let path = dir.join(format!("{name}.stt-native.lock"));
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "native ownership marker is not an ordinary file",
                ));
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        file.try_lock().map_err(std::io::Error::from)?;
        Ok(Self(file))
    }
}
impl Drop for NativeLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
impl Drop for NativeExit {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let _ = self.notice.send(true);
    }
}

fn failure(message: impl Into<String>) -> SttError {
    SttError::new(SttErrorClass::Server, "handoff", message)
}
fn validate_cloud_input(cloud: &DeepgramStreamConfig, queued: usize) -> Result<(), SttError> {
    if queued < MAX_READ_SAMPLES {
        return Err(failure("cloud queue must hold one replay chunk"));
    }
    if cloud.params.sample_rate != 16000
        || cloud.params.channels != 1
        || cloud.params.encoding != "linear16"
        || cloud.normalizer.model != cloud.params.model
    {
        return Err(SttError::new(
            SttErrorClass::BadRequest,
            "handoff",
            "handoff requires canonical 16 kHz mono PCM16 and matching provider model metadata",
        ));
    }
    Ok(())
}
fn source(source: Source) -> ReplaySource {
    match source {
        Source::System => ReplaySource::System,
        Source::Mic => ReplaySource::Mic,
    }
}
impl Shared {
    fn emit(&self, event: Event) {
        let gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        if !gate.fenced {
            let _ = self.events.send(event);
        }
    }
    fn fenced(&self) -> bool {
        self.gate.lock().unwrap_or_else(|p| p.into_inner()).fenced
    }
    fn terminal(&self, error: Option<SttError>) {
        let mut gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        if gate.fenced {
            return;
        }
        gate.status.phase = if error.is_some() {
            Phase::Incomplete
        } else if self.prefix_only {
            Phase::PrefixRecovered
        } else {
            Phase::Complete
        };
        gate.status.error = error.as_ref().map(|e| e.message.clone());
        if let Some(error) = error {
            let _ = self
                .events
                .send(Event::Transcript(StreamEvent::Error(error)));
        }
        let _ = self
            .events
            .send(Event::Transcript(StreamEvent::State(StreamState::Closed)));
        gate.fenced = true;
    }
    fn commit_status(&self, journal: &CoverageJournal) {
        let mut gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        if !gate.fenced {
            gate.status.committed_sample = journal.committed_through();
        }
    }
}

/// Live source handle. `write` belongs on the blocking capture pump, not an
/// audio callback. `finish` is async and fences publication at its deadline.
pub struct FailoverStream {
    shared: Arc<Shared>,
    done: watch::Receiver<bool>,
    published: watch::Receiver<bool>,
    finish_on_publication: bool,
    stop_timeout: Duration,
}

/// An already allocated native state. Constructed before capture admission;
/// opening the source binds its eventual WAL identity without reallocating.
pub struct PreparedSource {
    config: FailoverConfig,
    recognizer: Box<dyn Recognizer>,
}

impl PreparedSource {
    /// Declare microphone processing after local preflight but before source
    /// creation. An existing declaration cannot silently change its identity.
    pub fn with_processing(
        mut self,
        descriptor: super::processing::Descriptor,
    ) -> Result<Self, SttError> {
        descriptor.validate().map_err(|e| failure(e.to_string()))?;
        if self
            .config
            .processing
            .as_ref()
            .is_some_and(|old| old != &descriptor)
        {
            return Err(failure("prepared audio processor identity changed"));
        }
        self.config.processing = Some(descriptor);
        Ok(self)
    }
    /// Bind a prepared source to the capture's authoritative clock and identity.
    pub async fn open(
        mut self,
        dir: &Path,
        session_id: &str,
        offset_ms: u64,
    ) -> Result<(FailoverStream, mpsc::UnboundedReceiver<Event>), SttError> {
        self.config.cloud.normalizer.session_id = session_id.to_owned();
        self.config.cloud.session_offset_ms = offset_ms;
        FailoverStream::open_prepared(dir, self).await
    }
}

impl FailoverStream {
    /// Open a cloud-only source with exact durable input and no local plan.
    /// Terminal provider failure retains input and reports incomplete
    /// transcription; handoff budgets never terminate cloud-only retries.
    pub async fn open_cloud(
        dir: &Path,
        config: CloudSourceConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        validate_cloud_input(&config.cloud, config.cloud_queue_samples)?;
        if let Some(processing) = config.processing.as_ref() {
            processing.validate().map_err(|e| failure(e.to_string()))?;
        }
        let session = config.cloud.normalizer.session_id.clone();
        let leg = config.cloud.normalizer.source;
        let anchor = config
            .cloud
            .session_offset_ms
            .checked_mul(16)
            .ok_or_else(|| failure("source clock overflow"))?;
        let initial = EngineDescriptor {
            provider: "deepgram".into(),
            model: config.cloud.normalizer.model.clone(),
            revision: None,
            capabilities: SttCapabilities::deepgram(),
        };
        let processing_descriptor = config.processing;
        let directory = dir.to_owned();
        let inputs = tokio::task::spawn_blocking(move || {
            let writer = match processing_descriptor.as_ref() {
                Some(descriptor) => ReplayWriter::create_with_processing(
                    &directory,
                    &session,
                    source(leg),
                    anchor,
                    &descriptor.fingerprint()?,
                ),
                None => ReplayWriter::create(&directory, &session, source(leg), anchor),
            }?;
            let processing = processing_descriptor
                .map(|descriptor| {
                    super::processing::Journal::create(
                        &directory,
                        &session,
                        source(leg),
                        anchor,
                        descriptor,
                    )
                })
                .transpose()?;
            let journal =
                CoverageJournal::create_with_fallback(&directory, &session, leg, initial, None)?;
            let reader = ReplayReader::open(&directory, &session, source(leg))?;
            Ok::<_, std::io::Error>(SourceInputs {
                native: None,
                processing,
                writer,
                journal,
                reader,
                recognizer: None,
            })
        })
        .await
        .map_err(|e| failure(e.to_string()))?
        .map_err(|e| failure(e.to_string()))?;
        let worker = WorkerConfig {
            session_id: config.cloud.normalizer.session_id.clone(),
            source: leg,
            local: None,
            endpoint: EndpointConfig::default(),
            limits: HandoffLimits::default(),
            cloud_queue_samples: config.cloud_queue_samples,
            cloud_settle: config.cloud_settle,
            stop_timeout: config.stop_timeout,
            prefix_end: None,
        };
        let transport = DeepgramStream::open(config.cloud.with_confirmed_finals());
        Self::launch(worker, inputs, Some(transport), false)
    }
    /// Whether this source accepts only provenance-bearing processed intervals.
    #[must_use]
    pub fn processing_declared(&self) -> bool {
        self.shared
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .processing
            .is_some()
    }
    /// Open durable local input before loading the recognizer. Model failure
    /// leaves the recording and canonical input available, with an error event.
    /// This path cannot construct cloud transport.
    pub async fn open_local(
        dir: &Path,
        config: LocalSourceConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        let plan = config.plan();
        plan.validate().map_err(|e| failure(e.to_string()))?;
        if let Some(processing) = config.processing.as_ref() {
            processing.validate().map_err(|e| failure(e.to_string()))?;
        }
        let anchor = config
            .stream
            .session_offset_ms
            .checked_mul(16)
            .ok_or_else(|| failure("source clock overflow"))?;
        let directory = dir.to_owned();
        let session = config.stream.session_id.clone();
        let leg = config.stream.source;
        let processing_descriptor = config.processing;
        let saved_plan = plan.clone();
        let inputs = tokio::task::spawn_blocking(move || {
            let native = NativeLease::acquire(&directory, leg)?;
            let writer = match processing_descriptor.as_ref() {
                Some(descriptor) => ReplayWriter::create_with_processing(
                    &directory,
                    &session,
                    source(leg),
                    anchor,
                    &descriptor.fingerprint()?,
                ),
                None => ReplayWriter::create(&directory, &session, source(leg), anchor),
            }?;
            let processing = processing_descriptor
                .map(|descriptor| {
                    super::processing::Journal::create(
                        &directory,
                        &session,
                        source(leg),
                        anchor,
                        descriptor,
                    )
                })
                .transpose()?;
            let journal = CoverageJournal::create_with_fallback(
                &directory,
                &session,
                leg,
                saved_plan.engine.clone(),
                Some(saved_plan),
            )?;
            let reader = ReplayReader::open(&directory, &session, source(leg))?;
            Ok::<_, std::io::Error>(SourceInputs {
                native: Some(native),
                writer,
                journal,
                reader,
                recognizer: None,
                processing,
            })
        })
        .await
        .map_err(|e| failure(e.to_string()))?
        .map_err(|e| failure(e.to_string()))?;
        Self::launch(
            WorkerConfig {
                session_id: config.stream.session_id,
                source: leg,
                endpoint: plan.endpoint,
                local: Some(LocalRuntime {
                    factory: config.local,
                    plan,
                }),
                limits: HandoffLimits::default(),
                cloud_queue_samples: 0,
                cloud_settle: Duration::ZERO,
                stop_timeout: config.stop_timeout,
                prefix_end: None,
            },
            inputs,
            None,
            true,
        )
    }
    /// Append one processor-owned interval on the canonical source clock.
    /// Must run on the blocking pump after original capture has been archived.
    /// Invalid calls accept no samples; disk errors fence provider publication.
    pub fn write_processed(&self, output: &fotw_pipeline::aec::Output) -> Result<(), SttError> {
        if output.samples.is_empty()
            || output.samples.len() > 16000
            || output.samples.iter().any(|s| !s.is_finite())
        {
            return Err(failure("invalid processed input"));
        }
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(failure("source input is frozen"));
        }
        let mut writer = self.shared.writer.lock().unwrap_or_else(|p| p.into_inner());
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(failure("source input is frozen"));
        }
        let processing = writer
            .processing
            .as_mut()
            .ok_or_else(|| failure("audio processing was not declared before capture"))?;
        processing
            .check(output.epoch, output.start_sample, output.samples.len())
            .map_err(|e| failure(e.to_string()))?;
        let pcm = fotw_pipeline::resample::Downmixer::to_i16(&output.samples);
        let result = processing
            .record(output.epoch, output.start_sample, output.kind, &pcm)
            .and_then(|()| processing.sync())
            .and_then(|()| writer.audio.append(&pcm))
            .and_then(|()| {
                if writer.last_sync.elapsed() >= Duration::from_secs(1) {
                    writer.sync()?;
                }
                Ok(())
            });
        if let Err(error) = result {
            let error = failure(error.to_string());
            self.shared.terminal(Some(error.clone()));
            return Err(error);
        }
        self.shared
            .end
            .store(writer.audio.end_sample(), Ordering::Release);
        self.shared.wake.notify_one();
        Ok(())
    }
    /// Prepare durable source files and preflight the real local model before
    /// opening cloud transport. Session admission must call this before capture
    /// and wire pending-audio retirement/deletion before enabling the mode.
    pub async fn open(
        dir: &Path,
        config: FailoverConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        let prepared = Self::prepare(config).await?;
        Self::open_prepared(dir, prepared).await
    }

    /// Allocate and retain the actual local state before opening any capture or
    /// cloud transport. Failure refuses fallback admission.
    pub async fn prepare(config: FailoverConfig) -> Result<PreparedSource, SttError> {
        validate_cloud_input(&config.cloud, config.cloud_queue_samples)?;
        config
            .local_plan()
            .validate()
            .map_err(|e| failure(e.to_string()))?;
        if let Some(processing) = config.processing.as_ref() {
            processing.validate().map_err(|e| failure(e.to_string()))?;
        }
        let local = Arc::clone(&config.local);
        let recognizer = tokio::task::spawn_blocking(move || local.create())
            .await
            .map_err(|e| failure(e.to_string()))??;
        Ok(PreparedSource { config, recognizer })
    }

    async fn open_prepared(
        dir: &Path,
        prepared: PreparedSource,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        let PreparedSource { config, recognizer } = prepared;
        let session_id = config.cloud.normalizer.session_id.clone();
        let leg = config.cloud.normalizer.source;
        let anchor = config
            .cloud
            .session_offset_ms
            .checked_mul(16)
            .ok_or_else(|| failure("source clock overflow"))?;
        let initial = EngineDescriptor {
            provider: "deepgram".into(),
            model: config.cloud.normalizer.model.clone(),
            revision: None,
            capabilities: SttCapabilities::deepgram(),
        };
        let fallback = config.local_plan();
        let processing_descriptor = config.processing.clone();
        let directory = dir.to_owned();
        let (writer, journal, reader, recognizer, processing, native) =
            tokio::task::spawn_blocking(move || {
                let native =
                    NativeLease::acquire(&directory, leg).map_err(|e| failure(e.to_string()))?;
                let writer = match processing_descriptor.as_ref() {
                    Some(descriptor) => ReplayWriter::create_with_processing(
                        &directory,
                        &session_id,
                        source(leg),
                        anchor,
                        &descriptor
                            .fingerprint()
                            .map_err(|e| failure(e.to_string()))?,
                    ),
                    None => ReplayWriter::create(&directory, &session_id, source(leg), anchor),
                }
                .map_err(|e| failure(e.to_string()))?;
                let processing = processing_descriptor
                    .map(|descriptor| {
                        super::processing::Journal::create(
                            &directory,
                            &session_id,
                            source(leg),
                            anchor,
                            descriptor,
                        )
                    })
                    .transpose()
                    .map_err(|e| failure(e.to_string()))?;
                let journal = CoverageJournal::create_with_fallback(
                    &directory,
                    &session_id,
                    leg,
                    initial,
                    Some(fallback),
                )
                .map_err(|e| failure(e.to_string()))?;
                let reader = ReplayReader::open(&directory, &session_id, source(leg))
                    .map_err(|e| failure(e.to_string()))?;
                Ok::<_, SttError>((writer, journal, reader, recognizer, processing, native))
            })
            .await
            .map_err(|e| failure(e.to_string()))??;
        let (cloud, cloud_events) =
            DeepgramStream::open(config.cloud.clone().with_confirmed_finals());
        let worker_config = WorkerConfig {
            session_id: config.cloud.normalizer.session_id.clone(),
            source: config.cloud.normalizer.source,
            endpoint: config.endpoint,
            local: Some(LocalRuntime {
                factory: Arc::clone(&config.local),
                plan: config.local_plan(),
            }),
            limits: config.limits,
            cloud_queue_samples: config.cloud_queue_samples,
            cloud_settle: config.cloud_settle,
            stop_timeout: config.stop_timeout,
            prefix_end: None,
        };
        Self::launch(
            worker_config,
            SourceInputs {
                native: Some(native),
                processing,
                writer,
                journal,
                reader,
                recognizer: Some(recognizer),
            },
            Some((cloud, cloud_events)),
            false,
        )
    }

    /// Recover only pending local speech against a frozen canonical file. This
    /// path cannot construct cloud transport or accept new live input. A busy
    /// native owner, unavailable/mismatched model or corrupt journal is refused.
    pub async fn resume_local(
        dir: &Path,
        request: LocalResumeConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        Self::resume_saved(dir, request, None).await
    }

    async fn resume_saved(
        dir: &Path,
        request: LocalResumeConfig,
        prefix_end: Option<u64>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        let directory = dir.to_owned();
        let (config, inputs) = tokio::task::spawn_blocking(move || {
            let native = NativeLease::acquire(&directory, request.source)
                .map_err(|e| failure(e.to_string()))?;
            let prefix_writer = if prefix_end.is_some() {
                Some(
                    ReplayWriter::resume_prefix_validated(
                        &directory,
                        &request.session_id,
                        source(request.source),
                        |reader| {
                            super::processing::validate(
                                &directory,
                                &request.session_id,
                                source(request.source),
                                reader,
                            )
                            .map(|_| ())
                        },
                    )
                    .map_err(|e| failure(e.to_string()))?,
                )
            } else {
                None
            };
            let snapshot =
                CoverageJournal::snapshot(&directory, &request.session_id, request.source)
                    .map_err(|e| failure(e.to_string()))?;
            let previous_through = snapshot
                .commits
                .last()
                .map_or(snapshot.epochs[0].from_sample, |c| c.through_sample);
            let plan = snapshot.fallback.ok_or_else(|| {
                failure("saved local recovery plan is unavailable; source audio retained")
            })?;
            if plan.engine.model != request.local.model()
                || plan.engine.revision != request.revision
                || plan.language.as_deref() != request.local.language()
            {
                return Err(failure(
                    "installed recovery model, revision or language does not match the saved plan",
                ));
            }
            let mut writer = match prefix_writer {
                Some(writer) => writer,
                None => ReplayWriter::resume_validated(
                    &directory,
                    &request.session_id,
                    source(request.source),
                    |reader| {
                        super::processing::validate(
                            &directory,
                            &request.session_id,
                            source(request.source),
                            reader,
                        )
                        .map(|_| ())
                    },
                )
                .map_err(|e| failure(e.to_string()))?,
            };
            let end = prefix_end.unwrap_or(writer.end_sample());
            if previous_through > end || end > writer.end_sample() {
                return Err(failure("requested prefix is outside saved coverage/audio"));
            }
            let mut journal =
                CoverageJournal::open(&directory, &request.session_id, request.source)
                    .map_err(|e| failure(e.to_string()))?;
            if journal.committed_through() > end || end > writer.end_sample() {
                return Err(failure("saved coverage exceeds frozen audio"));
            }
            let pending = journal.committed_through() < end;
            let recognizer = if pending {
                Some(request.local.create()?)
            } else {
                None
            };
            if prefix_end.is_none() {
                writer.seal().map_err(|e| failure(e.to_string()))?;
            }
            if pending {
                journal
                    .switch(plan.engine.clone(), SwitchReason::ResumePending)
                    .map_err(|e| failure(e.to_string()))?;
            }
            let reader =
                ReplayReader::open(&directory, &request.session_id, source(request.source))
                    .map_err(|e| failure(e.to_string()))?;
            let config = WorkerConfig {
                session_id: request.session_id,
                source: request.source,
                endpoint: plan.endpoint,
                local: Some(LocalRuntime {
                    factory: request.local,
                    plan,
                }),
                limits: HandoffLimits::default(),
                cloud_queue_samples: 0,
                cloud_settle: Duration::ZERO,
                stop_timeout: request.stop_timeout,
                prefix_end,
            };
            Ok::<_, SttError>((
                config,
                SourceInputs {
                    native: Some(native),
                    processing: None,
                    writer,
                    journal,
                    reader,
                    recognizer,
                },
            ))
        })
        .await
        .map_err(|e| failure(e.to_string()))??;
        Self::launch(config, inputs, None, false)
    }

    /// Recover an explicitly bounded canonical prefix without closing the
    /// recording or certifying its remaining audio. Successful status is
    /// `PrefixRecovered`; `finish` still returns false for whole-input completion.
    pub async fn resume_prefix(
        dir: &Path,
        request: LocalResumeConfig,
        through_sample: u64,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        Self::resume_saved(dir, request, Some(through_sample)).await
    }

    fn launch(
        config: WorkerConfig,
        inputs: SourceInputs,
        transport: Option<(DeepgramStream, mpsc::UnboundedReceiver<StreamEvent>)>,
        live_local: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        let SourceInputs {
            native,
            processing,
            writer,
            journal,
            reader,
            recognizer,
        } = inputs;
        let anchor = journal.committed_through();
        let end = config.prefix_end.unwrap_or(writer.end_sample());
        let local = transport.is_none();
        let resuming = local && !live_local;
        let (cloud, cloud_events) = match transport {
            Some((cloud, events)) => (Some(Arc::new(cloud)), Some(events)),
            None => (None, None),
        };
        let abort_on_failure = cloud.clone();
        let (events, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            writer: Mutex::new(Writer {
                processing,
                audio: writer,
                last_sync: Instant::now(),
            }),
            end: Arc::new(AtomicU64::new(end)),
            stop_requested: AtomicBool::new(resuming),
            input_closed: AtomicBool::new(resuming),
            done: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(Mutex::new(Publication {
                fenced: false,
                status: Status {
                    phase: if local { Phase::Local } else { Phase::Cloud },
                    epoch: journal.current_epoch().clone(),
                    committed_sample: anchor,
                    written_sample: end,
                    lag_samples: end.saturating_sub(anchor),
                    worker_running: true,
                    error: None,
                },
            })),
            wake: Condvar::new(),
            wait: Mutex::new(()),
            events,
            prefix_only: config.prefix_end.is_some(),
        });
        if resuming && anchor < end {
            let _ = shared
                .events
                .send(Event::ProviderSwitch(journal.current_epoch().clone()));
        }
        let stop_timeout = config.stop_timeout;
        let (done_sender, done) = watch::channel(false);
        let (published_sender, published) = watch::channel(false);
        let worker_shared = Arc::clone(&shared);
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("fotw-durable-handoff".into())
            .spawn(move || {
                // Declared before moving native captures into locals: status
                // releases only after their destruction, including unwinding.
                let _native_exit = NativeExit {
                    done: Arc::clone(&worker_shared.done),
                    notice: done_sender,
                };
                let native = native;
                let config = config;
                let recognizer = recognizer;
                let mut worker = Worker {
                    policy: HandoffPolicy::new(config.limits),
                    vad: Endpointer::new(config.endpoint),
                    local_endpoint: local.then(|| Endpointer::new(config.endpoint)),
                    recognizer,
                    cloud,
                    cloud_events,
                    cloud_cursor: anchor,
                    vad_cursor: anchor,
                    local_cursor: anchor,
                    local_base: anchor,
                    blocked_since: None,
                    stop_since: None,
                    started: Instant::now(),
                    shared: Arc::clone(&worker_shared),
                    config,
                    journal,
                    reader,
                    runtime,
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if live_local {
                        worker.shared.emit(Event::Transcript(StreamEvent::State(
                            StreamState::Connecting,
                        )));
                        worker.recognizer = Some(
                            worker
                                .config
                                .local
                                .as_ref()
                                .ok_or_else(|| failure("local source has no local runtime"))?
                                .factory
                                .create()?,
                        );
                        worker
                            .shared
                            .emit(Event::Transcript(StreamEvent::State(StreamState::Open)));
                    }
                    worker.run()
                }))
                .unwrap_or_else(|_| Err(failure("transcription worker panicked; audio retained")));
                // Release disk/publication owners before native destruction.
                // A local-only Stop can finish its audio while the native tail
                // remains observable and prevents another recording.
                let native_state = worker.recognizer.take();
                let native_factory = worker
                    .config
                    .local
                    .as_ref()
                    .map(|local| Arc::clone(&local.factory));
                drop(worker);
                worker_shared.terminal(result.err());
                // Release the thread's canonical owner before waking a caller
                // that drops its stream and acquires projection/source leases.
                drop(worker_shared);
                let _ = published_sender.send(true);
                drop(native_state);
                drop(native_factory);
                drop(native);
            })
            .map_err(|e| {
                if let Some(cloud) = abort_on_failure {
                    cloud.abort();
                }
                failure(e.to_string())
            })?;
        Ok((
            Self {
                shared,
                done,
                published,
                finish_on_publication: live_local,
                stop_timeout,
            },
            receiver,
        ))
    }

    /// Append exact canonical audio before provider delivery. A failure leaves
    /// original capture independent and reports a pending transcript interval.
    pub fn write(&self, pcm: &[i16]) -> Result<(), SttError> {
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(failure("source input is frozen"));
        }
        let mut writer = self.shared.writer.lock().unwrap_or_else(|p| p.into_inner());
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(failure("source input is frozen"));
        }
        if writer.processing.is_some() {
            return Err(failure("declared audio processing requires tagged input"));
        }
        let result = writer.audio.append(pcm).and_then(|()| {
            if writer.last_sync.elapsed() >= Duration::from_secs(1) {
                writer.sync()?;
            }
            Ok(())
        });
        if let Err(error) = result {
            let error = failure(error.to_string());
            self.shared.terminal(Some(error.clone()));
            return Err(error);
        }
        self.shared
            .end
            .store(writer.audio.end_sample(), Ordering::Release);
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Freeze accepted input, sync its tail, and allow a bounded transcript
    /// drain. Late native output cannot publish after this deadline returns.
    pub async fn finish(&self) -> bool {
        self.shared.stop_requested.store(true, Ordering::Release);
        let shared = Arc::clone(&self.shared);
        let mut done = if self.finish_on_publication {
            self.published.clone()
        } else {
            self.done.clone()
        };
        let drain = async move {
            tokio::task::spawn_blocking(move || {
                let result = if shared.prefix_only {
                    Ok(0)
                } else {
                    shared
                        .writer
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .seal()
                };
                if let Err(error) = result {
                    shared.terminal(Some(failure(error.to_string())));
                }
                shared.input_closed.store(true, Ordering::Release);
                shared.wake.notify_one();
            })
            .await
            .map_err(|e| failure(e.to_string()))?;
            done.wait_for(|done| *done)
                .await
                .map_err(|e| failure(e.to_string()))?;
            Ok::<_, SttError>(())
        };
        match tokio::time::timeout(self.stop_timeout, drain).await {
            Ok(Ok(())) => self.status().phase == Phase::Complete,
            Ok(Err(error)) => {
                self.shared.terminal(Some(error));
                false
            }
            Err(_) => {
                self.shared.terminal(Some(failure(
                    "transcription drain timed out; audio retained for retry",
                )));
                false
            }
        }
    }

    /// Current provenance, durable coverage, and pending lag.
    #[must_use]
    pub fn status(&self) -> Status {
        StatusReader::snapshot(&self.shared.gate, &self.shared.end, &self.shared.done)
    }

    /// Observe lifecycle after stream drop without retaining model or file leases.
    #[must_use]
    pub fn status_reader(&self) -> StatusReader {
        StatusReader {
            gate: Arc::clone(&self.shared.gate),
            end: Arc::clone(&self.shared.end),
            done: Arc::clone(&self.shared.done),
        }
    }
}
impl Drop for FailoverStream {
    fn drop(&mut self) {
        self.shared.stop_requested.store(true, Ordering::Release);
        self.shared.terminal(Some(failure(
            "source dropped before transcription completed; audio retained",
        )));
        self.shared.input_closed.store(true, Ordering::Release);
        self.shared.wake.notify_one();
    }
}

struct Worker {
    shared: Arc<Shared>,
    config: WorkerConfig,
    journal: CoverageJournal,
    reader: ReplayReader,
    cloud: Option<Arc<DeepgramStream>>,
    cloud_events: Option<mpsc::UnboundedReceiver<StreamEvent>>,
    cloud_cursor: u64,
    vad_cursor: u64,
    local_cursor: u64,
    local_base: u64,
    vad: Endpointer,
    local_endpoint: Option<Endpointer>,
    recognizer: Option<Box<dyn Recognizer>>,
    policy: HandoffPolicy,
    started: Instant,
    blocked_since: Option<Instant>,
    stop_since: Option<Instant>,
    runtime: tokio::runtime::Handle,
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(cloud) = &self.cloud {
            cloud.abort();
        }
    }
}
impl Worker {
    fn now(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    fn run(&mut self) -> Result<(), SttError> {
        loop {
            if self.shared.fenced() {
                return Err(failure("source publication fenced"));
            }
            let mut moved = false;
            if self.cloud.is_some() {
                for _ in 0..64 {
                    let event = match self.cloud_events.as_mut().unwrap().try_recv() {
                        Ok(event) => event,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            self.demote_local(SwitchReason::TransportBudget)?;
                            break;
                        }
                    };
                    moved = true;
                    match event {
                        StreamEvent::Final(segment) => {
                            let through = segment
                                .end_ms
                                .checked_mul(16)
                                .ok_or_else(|| failure("provider clock overflow"))?;
                            if through > self.journal.committed_through() {
                                self.commit(through, vec![segment])?;
                                self.policy.confirmed_final(self.vad.voiced_samples());
                            }
                        }
                        StreamEvent::Error(error) => {
                            // Handoff budgets belong only to a selected backup.
                            // Cloud-only keeps transport retries but still surfaces
                            // terminal provider/request errors immediately.
                            let decision = if self.config.local.is_some() {
                                self.policy.error(self.now(), &error)
                            } else {
                                match error.class {
                                    SttErrorClass::Auth
                                    | SttErrorClass::Quota
                                    | SttErrorClass::BadRequest
                                    | SttErrorClass::Unsupported
                                    | SttErrorClass::AudioFormat => Decision::Surface,
                                    _ => Decision::KeepCloud,
                                }
                            };
                            if decision == Decision::Surface {
                                return Err(error);
                            }
                            self.shared
                                .emit(Event::Transcript(StreamEvent::Error(error)));
                            if let Decision::Switch(reason) = decision {
                                self.demote_local(reason)?;
                                break;
                            }
                        }
                        StreamEvent::State(StreamState::Closed) => {
                            self.demote_local(SwitchReason::TransportBudget)?;
                            break;
                        }
                        StreamEvent::State(state) => {
                            self.policy.state(self.now(), state);
                            self.shared
                                .emit(Event::Transcript(StreamEvent::State(state)));
                        }
                        partial => self.shared.emit(Event::Transcript(partial)),
                    }
                }
            }
            let end = self.shared.end.load(Ordering::Acquire);
            if self.cloud.is_some() {
                if self.vad_cursor < end {
                    let pcm = self.read(self.vad_cursor, end)?;
                    self.vad_cursor += pcm.len() as u64;
                    drop(self.vad.push(&pcm));
                    moved = true;
                }
                if self.config.local.is_some()
                    && let Decision::Switch(reason) =
                        self.policy.tick(self.now(), self.vad.voiced_samples())
                {
                    self.demote_local(reason)?;
                }
            }
            if self.cloud.is_some() {
                if self.cloud_cursor < end {
                    let pcm = self.read(self.cloud_cursor, end)?;
                    if self
                        .cloud
                        .as_ref()
                        .unwrap()
                        .write_bounded(&pcm, self.config.cloud_queue_samples)
                    {
                        self.cloud_cursor += pcm.len() as u64;
                        self.blocked_since = None;
                        moved = true;
                    } else if self.config.local.is_some()
                        && self
                            .blocked_since
                            .get_or_insert_with(Instant::now)
                            .elapsed()
                            .as_millis()
                            >= u128::from(self.config.limits.retry_budget_ms)
                    {
                        self.demote_local(SwitchReason::TransportBudget)?;
                    }
                }
                if self.cloud.is_some() && self.shared.input_closed.load(Ordering::Acquire) {
                    if self.journal.committed_through() == end {
                        return self.complete(end);
                    }
                    if self.stop_since.is_none() {
                        self.stop_since = Some(Instant::now());
                        let cloud = Arc::clone(self.cloud.as_ref().unwrap());
                        self.runtime.spawn(async move {
                            let _ =
                                tokio::time::timeout(Duration::from_secs(1), cloud.flush()).await;
                        });
                    }
                    if self.stop_since.unwrap().elapsed() >= self.config.cloud_settle {
                        self.demote_local(SwitchReason::FinishingTail)?;
                    }
                }
            }
            if self.cloud.is_none() {
                if self.local_cursor < end {
                    let pcm = self.read(self.local_cursor, end)?;
                    self.local_cursor += pcm.len() as u64;
                    let chunks = self.local_endpoint.as_mut().unwrap().push(&pcm);
                    for chunk in chunks {
                        self.infer(chunk)?;
                    }
                    let safe = self.local_base
                        + self.local_endpoint.as_ref().unwrap().safe_through_sample();
                    if safe > self.journal.committed_through() {
                        self.commit(safe, Vec::new())?;
                    }
                    moved = true;
                }
                if self.shared.input_closed.load(Ordering::Acquire) && self.local_cursor == end {
                    if let Some(chunk) = self.local_endpoint.as_mut().unwrap().flush() {
                        self.infer(chunk)?;
                    }
                    if end > self.journal.committed_through() {
                        self.commit(end, Vec::new())?;
                    }
                    return self.complete(end);
                }
            }
            if !moved {
                let guard = self.shared.wait.lock().unwrap_or_else(|p| p.into_inner());
                drop(
                    self.shared
                        .wake
                        .wait_timeout(guard, Duration::from_millis(10))
                        .unwrap_or_else(|p| p.into_inner()),
                );
            }
        }
    }
    fn read(&mut self, cursor: u64, end: u64) -> Result<Vec<i16>, SttError> {
        self.reader
            .read_from(cursor, (end - cursor).min(MAX_READ_SAMPLES as u64) as usize)
            .map_err(|e| failure(e.to_string()))
    }
    fn complete(&mut self, end: u64) -> Result<(), SttError> {
        if self.shared.fenced()
            || !self.shared.input_closed.load(Ordering::Acquire)
            || self.journal.committed_through() != end
        {
            return Err(failure(
                "source completion requires frozen, fully committed input",
            ));
        }
        if self.config.prefix_end == Some(end) {
            return Ok(());
        }
        self.reader
            .mark_complete(end)
            .map_err(|e| failure(e.to_string()))
    }
    fn demote_local(&mut self, reason: SwitchReason) -> Result<(), SttError> {
        let local=self.config.local.as_ref().ok_or_else(||failure("cloud transcription incomplete; original input retained; no local fallback was selected"))?;
        if let Some(cloud) = self.cloud.take() {
            cloud.abort();
        }
        self.cloud_events.take();
        let epoch = self
            .journal
            .switch(
                EngineDescriptor {
                    provider: fotw_stt::local::PROVIDER.into(),
                    model: local.factory.model().into(),
                    revision: local.plan.engine.revision.clone(),
                    capabilities: local.plan.engine.capabilities.clone(),
                },
                reason,
            )
            .map_err(|e| failure(e.to_string()))?;
        self.local_base = self.journal.committed_through();
        self.local_cursor = self.local_base;
        self.local_endpoint = Some(Endpointer::new(self.config.endpoint));
        let mut gate = self.shared.gate.lock().unwrap_or_else(|p| p.into_inner());
        if !gate.fenced {
            gate.status.phase = Phase::Local;
            gate.status.epoch = epoch.clone();
            let _ = self.shared.events.send(Event::ProviderSwitch(epoch));
        }
        drop(gate);
        if self.shared.fenced() {
            return Err(failure("late native initialization fenced"));
        }
        Ok(())
    }
    fn infer(&mut self, chunk: Chunk) -> Result<(), SttError> {
        if self.shared.fenced() {
            return Err(failure("native inference fenced before entry"));
        }
        let pcm: Vec<f32> = chunk
            .samples
            .iter()
            .map(|&sample| f32::from(sample) / 32768.0)
            .collect();
        let recognized = self.recognizer.as_mut().unwrap().transcribe(&pcm)?;
        if self.shared.fenced() {
            return Err(failure("late native inference fenced"));
        }
        let config = LocalStreamConfig::new(self.config.session_id.clone(), self.config.source)
            .with_session_offset_ms(self.local_base / 16);
        let local = self
            .config
            .local
            .as_ref()
            .ok_or_else(|| failure("local inference was not selected"))?;
        let segments = segments_for(
            &config,
            local.factory.model(),
            local.factory.language(),
            &chunk,
            recognized,
        );
        self.commit(
            self.local_base + chunk.start_sample + chunk.samples.len() as u64,
            segments,
        )
    }
    fn commit(&mut self, through: u64, segments: Vec<TranscriptSegment>) -> Result<(), SttError> {
        if self.shared.fenced() {
            return Err(failure("late coverage commit fenced"));
        }
        let result = self
            .journal
            .commit(
                self.journal.current_epoch().number,
                self.journal.committed_through(),
                through,
                segments.clone(),
            )
            .map_err(|e| failure(e.to_string()))?;
        self.shared.commit_status(&self.journal);
        if result == CommitOutcome::New {
            for segment in segments {
                self.shared
                    .emit(Event::Transcript(StreamEvent::Final(segment)));
            }
        }
        Ok(())
    }
}
