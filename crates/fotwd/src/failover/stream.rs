//! One durable cloud-to-local source. Disk IO and native inference run on a
//! dedicated worker; the capture pump only appends canonical audio. This is
//! not enabled in recording until pending-session retention is connected.

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
    CommitOutcome, CoverageJournal, EngineDescriptor, ProviderEpoch, SwitchReason,
};
use super::policy::{Decision, HandoffLimits, HandoffPolicy};

/// Per-source input and shutdown settings. Contains a cloud credential; never
/// serialize or log this object.
pub struct FailoverConfig {
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
    /// Absolute end of canonical audio accepted by the pump.
    pub written_sample: u64,
    /// Audio still awaiting transcription or silence acknowledgement.
    pub lag_samples: u64,
    /// Worker/native cleanup is still outstanding.
    pub worker_running: bool,
    /// An incomplete-transcript explanation.
    pub error: Option<String>,
}

struct Publication {
    fenced: bool,
    status: Status,
}
struct Writer {
    audio: ReplayWriter,
    last_sync: Instant,
}
struct Shared {
    writer: Mutex<Writer>,
    end: AtomicU64,
    stop_requested: AtomicBool,
    input_closed: AtomicBool,
    done: AtomicBool,
    gate: Mutex<Publication>,
    wake: Condvar,
    wait: Mutex<()>,
    events: mpsc::UnboundedSender<Event>,
}

fn failure(message: impl Into<String>) -> SttError {
    SttError::new(SttErrorClass::Server, "handoff", message)
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
    stop_timeout: Duration,
}

impl FailoverStream {
    /// Prepare durable source files and preflight the real local model before
    /// opening cloud transport. Session admission must call this before capture
    /// and wire pending-audio retirement/deletion before enabling the mode.
    pub async fn open(
        dir: &Path,
        config: FailoverConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), SttError> {
        if config.cloud_queue_samples < MAX_READ_SAMPLES {
            return Err(failure("cloud queue must hold one replay chunk"));
        }
        if config.cloud.params.sample_rate != 16_000
            || config.cloud.params.channels != 1
            || config.cloud.params.encoding != "linear16"
            || config.cloud.normalizer.model != config.cloud.params.model
        {
            return Err(SttError::new(
                SttErrorClass::BadRequest,
                "handoff",
                "handoff requires canonical 16 kHz mono PCM16 and matching provider model metadata",
            ));
        }
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
        let directory = dir.to_owned();
        let local = Arc::clone(&config.local);
        let (writer, journal, reader) = tokio::task::spawn_blocking(move || {
            drop(local.create()?); // load real weights/state before any cloud socket.
            let writer = ReplayWriter::create(&directory, &session_id, source(leg), anchor)
                .map_err(|e| failure(e.to_string()))?;
            let journal = CoverageJournal::create(&directory, &session_id, leg, initial)
                .map_err(|e| failure(e.to_string()))?;
            let reader = ReplayReader::open(&directory, &session_id, source(leg))
                .map_err(|e| failure(e.to_string()))?;
            Ok::<_, SttError>((writer, journal, reader))
        })
        .await
        .map_err(|e| failure(e.to_string()))??;
        let (events, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            writer: Mutex::new(Writer {
                audio: writer,
                last_sync: Instant::now(),
            }),
            end: AtomicU64::new(anchor),
            stop_requested: AtomicBool::new(false),
            input_closed: AtomicBool::new(false),
            done: AtomicBool::new(false),
            gate: Mutex::new(Publication {
                fenced: false,
                status: Status {
                    phase: Phase::Cloud,
                    epoch: journal.current_epoch().clone(),
                    committed_sample: anchor,
                    written_sample: anchor,
                    lag_samples: 0,
                    worker_running: true,
                    error: None,
                },
            }),
            wake: Condvar::new(),
            wait: Mutex::new(()),
            events,
        });
        let (cloud, cloud_events) =
            DeepgramStream::open(config.cloud.clone().with_confirmed_finals());
        let cloud = Arc::new(cloud);
        let abort_on_failure = Arc::clone(&cloud);
        let stop_timeout = config.stop_timeout;
        let (done_sender, done) = watch::channel(false);
        let worker_shared = Arc::clone(&shared);
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("fotw-durable-handoff".into())
            .spawn(move || {
                let mut worker = Worker {
                    policy: HandoffPolicy::new(config.limits),
                    vad: Endpointer::new(config.endpoint),
                    local_endpoint: None,
                    recognizer: None,
                    cloud: Some(cloud),
                    cloud_events: Some(cloud_events),
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
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run()))
                        .unwrap_or_else(|_| {
                            Err(failure("transcription worker panicked; audio retained"))
                        });
                drop(worker); // native state and journal ownership released before done.
                worker_shared.terminal(result.err());
                worker_shared.done.store(true, Ordering::Release);
                let _ = done_sender.send(true);
            })
            .map_err(|e| {
                abort_on_failure.abort();
                failure(e.to_string())
            })?;
        Ok((
            Self {
                shared,
                done,
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
        let result = writer.audio.append(pcm).and_then(|()| {
            if writer.last_sync.elapsed() >= Duration::from_secs(1) {
                writer.audio.sync()?;
                writer.last_sync = Instant::now();
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
        let mut done = self.done.clone();
        let drain = async move {
            tokio::task::spawn_blocking(move || {
                let result = shared
                    .writer
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .audio
                    .sync();
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
        let mut status = self
            .shared
            .gate
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status
            .clone();
        status.written_sample = self.shared.end.load(Ordering::Acquire);
        status.lag_samples = status
            .written_sample
            .saturating_sub(status.committed_sample);
        status.worker_running = !self.shared.done.load(Ordering::Acquire);
        status
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
    config: FailoverConfig,
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
                            let decision = self.policy.error(self.now(), &error);
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
                if let Decision::Switch(reason) =
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
                    } else if self
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
                        return Ok(());
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
                    return Ok(());
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
    fn demote_local(&mut self, reason: SwitchReason) -> Result<(), SttError> {
        if let Some(cloud) = self.cloud.take() {
            cloud.abort();
        }
        self.cloud_events.take();
        let epoch = self
            .journal
            .switch(
                EngineDescriptor {
                    provider: fotw_stt::local::PROVIDER.into(),
                    model: self.config.local.model().into(),
                    revision: self.config.local_revision.clone(),
                    capabilities: self.config.local_capabilities.clone(),
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
        self.recognizer = Some(self.config.local.create()?);
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
        let config = LocalStreamConfig::new(
            self.config.cloud.normalizer.session_id.clone(),
            self.config.cloud.normalizer.source,
        )
        .with_session_offset_ms(self.local_base / 16);
        let segments = segments_for(
            &config,
            self.config.local.model(),
            self.config.local.language(),
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
