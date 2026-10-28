//! The process owner: spawn, handshake, route, restart.
//!
//! One [`SpeechdClient`] owns at most one live helper at a time, a `Conn`.
//! Three tasks serve a connection: a writer that owns the helper's stdin, a
//! reader that owns its stdout and routes replies, and a reaper that owns the
//! child process. Whichever of them first sees the helper go (EOF, a write
//! error, an exit, a malformed frame, a missed deadline) fails the connection:
//! every pending request and every open stream gets the error at once, the
//! child is killed, and the client starts a replacement under its
//! [`RestartPolicy`]. Nothing waits on a dead helper.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::frame::{self, Frame, FrameError, MAX_SAMPLES_PER_FRAME};
use crate::message::{
    Capabilities, CloseReason, EngineKind, ErrorCode, FORMAT_S16LE, FinalText, PROTOCOL_VERSION,
    ProgressPhase, Reply, Request, StreamOptions, VadKind,
};
use crate::restart::{RestartBudget, RestartPolicy};

/// Frames queued for the helper's stdin before senders wait.
const WRITE_QUEUE: usize = 32;
/// After the helper exits, or stops taking input, how long the reader may
/// keep reading what it wrote before dying (a final, its own fatal error)
/// before the connection is failed anyway. Well inside the 1 s crash budget.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(250);

/// How to run the helper, and how long to wait for it.
#[derive(Debug, Clone)]
pub struct SpeechdConfig {
    /// The helper executable.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// Sent in `hello`, for the helper's diagnostics.
    pub client_name: String,
    /// Restarts after a crash.
    pub restart: RestartPolicy,
    /// Spawn to `hello` answered.
    pub handshake_timeout: Duration,
    /// Ordinary requests: open, health, unload.
    pub request_timeout: Duration,
    /// `load` and `warmup`, which may compile Core ML models.
    pub load_timeout: Duration,
    /// `install`, which may download hundreds of megabytes.
    pub install_timeout: Duration,
    /// How long [`SpeechStream::send_audio`] may wait for room in the queue to
    /// the helper. Past this the helper is unresponsive and is killed.
    pub audio_stall_timeout: Duration,
    /// While any stream is open the client asks the helper for `health` this
    /// often (1 s by default), so a helper that hangs mid-stream is found and
    /// killed instead of leaving a caller waiting for a final forever.
    pub heartbeat_interval: Duration,
    /// How long a heartbeat may go unanswered (2 s by default). With the
    /// interval this bounds how long a hung helper goes unnoticed: 3 s by
    /// default, even after `finish`, when no audio is flowing.
    pub heartbeat_timeout: Duration,
}

impl SpeechdConfig {
    /// `program serve --models-root <models_root>`, with the default deadlines.
    pub fn new(program: impl Into<PathBuf>, models_root: impl AsRef<Path>) -> Self {
        Self {
            program: program.into(),
            args: vec![
                "serve".into(),
                "--models-root".into(),
                models_root.as_ref().as_os_str().to_owned(),
            ],
            client_name: concat!("fotw-speechd-client/", env!("CARGO_PKG_VERSION")).to_owned(),
            restart: RestartPolicy::default(),
            handshake_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
            load_timeout: Duration::from_secs(180),
            install_timeout: Duration::from_secs(6 * 3600),
            audio_stall_timeout: Duration::from_secs(2),
            heartbeat_interval: Duration::from_secs(1),
            heartbeat_timeout: Duration::from_secs(2),
        }
    }

    /// Append one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClientError {
    /// The helper could not be started.
    #[error("could not start the speech helper: {0}")]
    Spawn(String),
    /// The helper speaks another protocol version. Not retried.
    #[error(
        "speech helper protocol mismatch: client {client}, helper {helper:?}, helper supports {supported:?}"
    )]
    VersionMismatch {
        /// This client's version.
        client: u32,
        /// The version the helper announced, if it got that far.
        helper: Option<u32>,
        /// The versions the helper says it supports, if it refused us.
        supported: Vec<u32>,
    },
    /// A malformed, oversized or unexpected frame.
    #[error("speech helper protocol error: {0}")]
    Protocol(String),
    /// The helper died.
    #[error("speech helper crashed: {0}")]
    HelperCrashed(String),
    /// The helper stopped answering and was killed.
    #[error("speech helper unresponsive: {0}")]
    HelperUnresponsive(String),
    /// The restart budget is spent; [`SpeechdClient::reset`] to try again.
    #[error("speech helper failed {attempts} restarts in a row")]
    HelperFailed {
        /// Restarts attempted.
        attempts: u32,
    },
    /// A request missed its deadline; the helper was killed.
    #[error("speech helper did not answer {0} in time")]
    Timeout(&'static str),
    /// The helper refused a request.
    #[error("speech helper refused: {code:?}: {message}")]
    Remote {
        /// Its code.
        code: ErrorCode,
        /// Its detail.
        message: String,
    },
    /// The helper serves no such engine.
    #[error("no speech engine {0:?}")]
    UnknownEngine(String),
    /// The engine does not take this format.
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    /// [`SpeechStream::try_send_audio`]: the queue to the helper is full.
    /// Nothing was sent; the caller decides whether to drop or retry.
    #[error("the speech helper is not keeping up; audio was not queued")]
    Backpressure,
    /// The stream was finished, cancelled or failed.
    #[error("the speech stream is closed")]
    StreamClosed,
    /// [`SpeechdClient::shutdown`] was called.
    #[error("the speech helper was shut down")]
    ShutDown,
}

impl ClientError {
    /// The wire code closest to this error.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::VersionMismatch { .. } => ErrorCode::VersionMismatch,
            Self::Protocol(_) => ErrorCode::Protocol,
            Self::Spawn(_) | Self::HelperCrashed(_) => ErrorCode::HelperCrashed,
            Self::HelperUnresponsive(_) => ErrorCode::HelperUnresponsive,
            Self::HelperFailed { .. } => ErrorCode::HelperFailed,
            Self::Timeout(_) => ErrorCode::Timeout,
            Self::Remote { code, .. } => *code,
            Self::UnknownEngine(_) => ErrorCode::UnknownEngine,
            Self::UnsupportedFormat(_) => ErrorCode::UnsupportedFormat,
            Self::StreamClosed | Self::ShutDown | Self::Backpressure => ErrorCode::Other,
        }
    }
}

/// Why a stream ended early.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamFailure {
    /// What happened.
    pub code: ErrorCode,
    /// Detail, for logs. Never transcript text.
    pub message: String,
}

/// What a stream yields.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Provisional text: display only, never persist.
    Partial {
        /// Text so far.
        text: String,
        /// Start, in ms from the stream's first sample.
        start_ms: u64,
        /// End, in ms from the stream's first sample.
        end_ms: u64,
        /// The utterance it belongs to; a later partial or final with the
        /// same id replaces it.
        utterance: Option<u64>,
    },
    /// Committed text.
    Final(FinalText),
    /// A voice-activity event.
    Vad {
        /// What happened.
        event: VadKind,
        /// Sample index from the stream's first sample.
        sample: u64,
        /// Speech probability.
        probability: f32,
    },
    /// The stream ended normally. Terminal.
    Closed(CloseReason),
    /// The stream failed. Terminal.
    Failed(StreamFailure),
}

/// What to open a stream on.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Engine id.
    pub engine: String,
    /// Sample rate of the PCM to come.
    pub sample_rate: u32,
    /// Engine options.
    pub options: StreamOptions,
}

impl StreamConfig {
    /// Mono s16le at `sample_rate`, default options.
    pub fn new(engine: impl Into<String>, sample_rate: u32) -> Self {
        Self {
            engine: engine.into(),
            sample_rate,
            options: StreamOptions::default(),
        }
    }
}

/// A `health` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    /// Time since this helper process started.
    pub uptime: Duration,
    /// Streams open in the helper.
    pub streams: u32,
    /// Loaded engines.
    pub loaded: Vec<String>,
    /// Resident memory, if the helper reports it.
    pub rss_bytes: Option<u64>,
}

/// A `load` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// Model id.
    pub model: String,
    /// Model revision.
    pub revision: String,
    /// How long the load took in the helper.
    pub elapsed: Duration,
}

/// Progress of a slow request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// What it is doing now.
    pub phase: ProgressPhase,
    /// Bytes done, for a download or check.
    pub bytes: Option<u64>,
    /// Bytes in total, for a download or check.
    pub total: Option<u64>,
}

/// A snapshot of the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Helpers spawned so far, counting the first.
    pub generation: u64,
    /// Restarts since the last success.
    pub restart_attempts: u32,
    /// The restart budget is spent, or the client was shut down.
    pub failed: bool,
    /// When the most recent helpers were spawned (up to 16), oldest first.
    pub spawns: Vec<Instant>,
}

// ------------------------------------------------------------------ client

/// Owns the helper process. Cheap to clone; clones share the helper.
#[derive(Clone)]
pub struct SpeechdClient {
    shared: Arc<Shared>,
}

struct Shared {
    config: SpeechdConfig,
    /// Serializes spawning, so concurrent callers share one restart.
    spawn_lock: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    /// Set by `shutdown`. Every step of a restart (its spacing, the spawn
    /// and handshake, the replayed loads) gives up as soon as it is.
    stop: tokio::sync::watch::Sender<bool>,
}

struct State {
    current: Option<Arc<Conn>>,
    /// A helper a restart has spawned but not finished replaying loads into,
    /// so `shutdown` can kill it without waiting for the restart.
    starting: Option<Arc<Conn>>,
    budget: RestartBudget,
    failed: Option<ClientError>,
    generation: u64,
    spawns: std::collections::VecDeque<Instant>,
    /// Every engine the app loaded, and whether it warmed it, to replay after
    /// a restart.
    wanted: Vec<(String, bool)>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let current = lock(&self.state).current.take();
        if let Some(conn) = current {
            conn.close(ClientError::ShutDown);
        }
    }
}

impl State {
    fn spawned(&mut self, at: Instant) {
        self.budget.spawned(at);
        if self.spawns.len() == 16 {
            self.spawns.pop_front();
        }
        self.spawns.push_back(at);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl SpeechdClient {
    /// Spawn the helper and complete the handshake. A failure here is
    /// returned, not retried.
    ///
    /// # Errors
    /// [`ClientError::Spawn`], [`ClientError::VersionMismatch`],
    /// [`ClientError::Protocol`], or a crash or timeout during the handshake.
    pub async fn start(config: SpeechdConfig) -> Result<Self, ClientError> {
        let budget = RestartBudget::new(config.restart);
        let shared = Arc::new(Shared {
            config,
            spawn_lock: tokio::sync::Mutex::new(()),
            stop: tokio::sync::watch::Sender::new(false),
            state: Mutex::new(State {
                current: None,
                starting: None,
                budget,
                failed: None,
                generation: 1,
                spawns: std::collections::VecDeque::new(),
                wanted: Vec::new(),
            }),
        });
        lock(&shared.state).spawned(Instant::now());
        let conn = Conn::spawn(&shared.config, 1, Arc::downgrade(&shared)).await?;
        lock(&shared.state).current = Some(conn);
        Ok(Self { shared })
    }

    /// The live helper's capabilities, if one is running.
    #[must_use]
    pub fn capabilities(&self) -> Option<Capabilities> {
        self.live().map(|conn| conn.capabilities.clone())
    }

    /// Load an engine's model from local files. It is loaded again after any
    /// restart.
    ///
    /// # Errors
    /// The helper's refusal (for example `model_missing`) or a helper failure.
    pub async fn load(&self, engine: &str) -> Result<Loaded, ClientError> {
        self.load_with_progress(engine, None).await
    }

    /// [`load`](Self::load), reporting `progress` events as they arrive. A
    /// first load on a machine includes Core ML's compile, which can take tens
    /// of seconds.
    ///
    /// # Errors
    /// As [`load`](Self::load).
    pub async fn load_with_progress(
        &self,
        engine: &str,
        progress: Option<mpsc::UnboundedSender<Progress>>,
    ) -> Result<Loaded, ClientError> {
        let conn = self.conn().await?;
        let reply = conn
            .load(engine, self.shared.config.load_timeout, progress)
            .await?;
        let mut state = lock(&self.shared.state);
        if !state.wanted.iter().any(|(e, _)| e == engine) {
            state.wanted.push((engine.to_owned(), false));
        }
        Ok(reply)
    }

    /// Run one silent chunk through a loaded engine, so the first real request
    /// does not pay for Core ML compilation. Repeated after any restart.
    ///
    /// # Errors
    /// As [`load`](Self::load).
    pub async fn warmup(&self, engine: &str) -> Result<Duration, ClientError> {
        self.warmup_with_progress(engine, None).await
    }

    /// [`warmup`](Self::warmup), reporting `progress` events as they arrive.
    ///
    /// # Errors
    /// As [`load`](Self::load).
    pub async fn warmup_with_progress(
        &self,
        engine: &str,
        progress: Option<mpsc::UnboundedSender<Progress>>,
    ) -> Result<Duration, ClientError> {
        let conn = self.conn().await?;
        let elapsed = conn
            .warmup(engine, self.shared.config.load_timeout, progress)
            .await?;
        if let Some(wanted) = lock(&self.shared.state)
            .wanted
            .iter_mut()
            .find(|(e, _)| e == engine)
        {
            wanted.1 = true;
        }
        Ok(elapsed)
    }

    /// Release an engine's model.
    ///
    /// # Errors
    /// As [`load`](Self::load).
    pub async fn unload(&self, engine: &str) -> Result<(), ClientError> {
        lock(&self.shared.state).wanted.retain(|(e, _)| e != engine);
        let conn = self.conn().await?;
        let engine = engine.to_owned();
        conn.call(
            move |id| Request::Unload { id, engine },
            self.shared.config.request_timeout,
            None,
            "unload",
        )
        .await
        .map(|_| ())
    }

    /// Open a stream. The engine and sample rate are checked against the
    /// helper's capabilities before anything is sent.
    ///
    /// # Errors
    /// [`ClientError::UnknownEngine`], [`ClientError::UnsupportedFormat`], the
    /// helper's refusal, or a helper failure.
    pub async fn open_stream(&self, config: StreamConfig) -> Result<SpeechStream, ClientError> {
        let conn = self.conn().await?;
        let engine = conn
            .capabilities
            .engine(&config.engine)
            .ok_or_else(|| ClientError::UnknownEngine(config.engine.clone()))?;
        if !engine.sample_rates.contains(&config.sample_rate) {
            return Err(ClientError::UnsupportedFormat(format!(
                "{} takes {:?} Hz, not {}",
                engine.id, engine.sample_rates, config.sample_rate
            )));
        }
        let stream = conn.next_stream.fetch_add(1, Ordering::Relaxed);
        let events = conn.route(stream)?;
        let result = conn
            .call(
                move |id| Request::OpenStream {
                    id,
                    stream,
                    engine: config.engine,
                    sample_rate: config.sample_rate,
                    format: FORMAT_S16LE.to_owned(),
                    channels: 1,
                    options: config.options,
                },
                self.shared.config.request_timeout,
                None,
                "open_stream",
            )
            .await;
        match result {
            Ok(Reply::StreamOpened { stream: opened, .. }) if opened == stream => {
                Ok(SpeechStream {
                    id: stream,
                    conn,
                    events,
                    finished: false,
                    cancelled: false,
                })
            }
            Ok(other) => {
                conn.unroute(stream);
                Err(conn.unexpected(&other))
            }
            Err(e) => {
                conn.unroute(stream);
                Err(e)
            }
        }
    }

    /// Ask the helper how it is.
    ///
    /// # Errors
    /// A helper failure.
    pub async fn health(&self) -> Result<Health, ClientError> {
        let conn = self.conn().await?;
        match conn
            .call(
                |id| Request::Health { id },
                self.shared.config.request_timeout,
                None,
                "health",
            )
            .await?
        {
            Reply::Health {
                uptime_ms,
                streams,
                loaded,
                rss_bytes,
                ..
            } => Ok(Health {
                uptime: Duration::from_millis(uptime_ms),
                streams,
                loaded,
                rss_bytes,
            }),
            other => Err(conn.unexpected(&other)),
        }
    }

    /// Explicit model setup: download a pinned model, or import it from a
    /// local directory. Never call this during capture.
    ///
    /// # Errors
    /// `install_failed` (the previous selection stands) or a helper failure.
    pub async fn install(
        &self,
        model: &str,
        from: Option<&Path>,
        progress: Option<mpsc::UnboundedSender<Progress>>,
    ) -> Result<String, ClientError> {
        let conn = self.conn().await?;
        let model = model.to_owned();
        let from = from.map(|p| p.to_string_lossy().into_owned());
        match conn
            .call(
                move |id| Request::Install { id, model, from },
                self.shared.config.install_timeout,
                progress,
                "install",
            )
            .await?
        {
            Reply::Installed { revision, .. } => Ok(revision),
            other => Err(conn.unexpected(&other)),
        }
    }

    /// A snapshot of the supervisor.
    #[must_use]
    pub fn status(&self) -> Status {
        let state = lock(&self.shared.state);
        Status {
            generation: state.generation,
            restart_attempts: state.budget.attempts(),
            failed: state.failed.is_some(),
            spawns: state.spawns.iter().copied().collect(),
        }
    }

    /// Clear a spent restart budget; the next call starts a helper again.
    pub async fn reset(&self) {
        let _spawning = self.shared.spawn_lock.lock().await;
        let mut state = lock(&self.shared.state);
        if matches!(state.failed, Some(ClientError::HelperFailed { .. })) {
            state.failed = None;
        }
        state.budget.succeeded();
    }

    /// Ask the helper to exit, then make sure it has. Every later call fails
    /// with [`ClientError::ShutDown`].
    pub async fn shutdown(&self) {
        let starting = {
            let mut state = lock(&self.shared.state);
            state.failed = Some(ClientError::ShutDown);
            state.starting.take()
        };
        // A restart may be in progress, holding the lock: stop it at whatever
        // step it is on, and kill the helper it is replaying loads into.
        self.shared.stop.send_replace(true);
        if let Some(conn) = starting {
            conn.close(ClientError::ShutDown);
        }
        let _spawning = self.shared.spawn_lock.lock().await;
        let conn = lock(&self.shared.state).current.take();
        if let Some(conn) = conn {
            conn.closing.store(true, Ordering::SeqCst);
            let _ = conn
                .call(
                    |id| Request::Shutdown { id },
                    Duration::from_secs(2),
                    None,
                    "shutdown",
                )
                .await;
            conn.close(ClientError::ShutDown);
        }
    }

    fn live(&self) -> Option<Arc<Conn>> {
        lock(&self.shared.state)
            .current
            .clone()
            .filter(|conn| conn.is_alive())
    }

    /// The live helper, restarting one if the last died.
    async fn conn(&self) -> Result<Arc<Conn>, ClientError> {
        if let Some(conn) = self.live() {
            return Ok(conn);
        }
        let _spawning = self.shared.spawn_lock.lock().await;
        loop {
            let (wait, generation) = {
                let mut state = lock(&self.shared.state);
                if let Some(conn) = state.current.clone().filter(|c| c.is_alive()) {
                    return Ok(conn);
                }
                if let Some(failed) = &state.failed {
                    return Err(failed.clone());
                }
                match state.budget.take(Instant::now()) {
                    Ok(wait) => {
                        state.generation += 1;
                        (wait, state.generation)
                    }
                    Err(spent) => {
                        let failed = ClientError::HelperFailed {
                            attempts: spent.attempts,
                        };
                        state.failed = Some(failed.clone());
                        return Err(failed);
                    }
                }
            };
            if self.until_stopped(tokio::time::sleep(wait)).await.is_none() {
                return Err(ClientError::ShutDown);
            }
            if let Some(failed) = &lock(&self.shared.state).failed {
                return Err(failed.clone());
            }
            lock(&self.shared.state).spawned(Instant::now());
            // Dropping an unfinished spawn kills its child (kill_on_drop).
            let spawned = self
                .until_stopped(Conn::spawn(
                    &self.shared.config,
                    generation,
                    Arc::downgrade(&self.shared),
                ))
                .await;
            let conn = match spawned {
                None => return Err(ClientError::ShutDown),
                Some(Ok(conn)) => conn,
                Some(Err(e @ ClientError::VersionMismatch { .. })) => {
                    lock(&self.shared.state).failed = Some(e.clone());
                    return Err(e);
                }
                Some(Err(_)) => continue,
            };
            {
                let mut state = lock(&self.shared.state);
                if let Some(failed) = &state.failed {
                    conn.close(failed.clone());
                    return Err(failed.clone());
                }
                state.starting = Some(conn.clone());
            }
            let replayed = self.until_stopped(self.replay(&conn)).await;
            let mut state = lock(&self.shared.state);
            state.starting = None;
            match replayed {
                None => {
                    conn.close(ClientError::ShutDown);
                    return Err(ClientError::ShutDown);
                }
                Some(Err(_)) => {
                    conn.close(ClientError::HelperCrashed(
                        "failed while reloading engines".into(),
                    ));
                    continue;
                }
                Some(Ok(())) => {}
            }
            if let Some(failed) = &state.failed {
                conn.close(failed.clone());
                return Err(failed.clone());
            }
            state.current = Some(conn.clone());
            return Ok(conn);
        }
    }

    /// Run `step` unless the client is shut down first; `None` if it was.
    async fn until_stopped<T>(&self, step: impl std::future::Future<Output = T>) -> Option<T> {
        let mut stop = self.shared.stop.subscribe();
        tokio::select! {
            value = step => Some(value),
            _ = stop.wait_for(|stopped| *stopped) => None,
        }
    }

    /// Load and warm what the app had before the restart. An engine the new
    /// helper refuses (its model was removed, say) is dropped from the list;
    /// only a helper failure fails the restart.
    async fn replay(&self, conn: &Arc<Conn>) -> Result<(), ClientError> {
        let wanted = lock(&self.shared.state).wanted.clone();
        let timeout = self.shared.config.load_timeout;
        for (engine, warm) in wanted {
            let mut result = conn.load(&engine, timeout, None).await.map(|_| ());
            if warm && result.is_ok() {
                result = conn.warmup(&engine, timeout, None).await.map(|_| ());
            }
            match result {
                Ok(()) => {}
                Err(ClientError::Remote { .. }) => {
                    lock(&self.shared.state)
                        .wanted
                        .retain(|(e, _)| *e != engine);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Shared {
    /// A live connection died on its own: start its replacement now, so the
    /// models are warm again before the next request needs them.
    fn conn_died(self: Arc<Self>, generation: u64) {
        let current = {
            let state = lock(&self.state);
            state.failed.is_none()
                && state
                    .current
                    .as_ref()
                    .is_some_and(|c| c.generation == generation)
        };
        if current && let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let client = SpeechdClient { shared: self };
            runtime.spawn(async move {
                let _ = client.conn().await;
            });
        }
    }

    fn stream_succeeded(&self) {
        lock(&self.state).budget.succeeded();
    }
}

// ------------------------------------------------------------------ stream

/// One stream on the helper. Dropping it cancels it.
pub struct SpeechStream {
    id: u32,
    conn: Arc<Conn>,
    events: mpsc::UnboundedReceiver<StreamEvent>,
    finished: bool,
    cancelled: bool,
}

impl std::fmt::Debug for SpeechStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeechStream")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SpeechStream {
    /// The stream id, unique for its helper process.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Send mono s16le PCM, split into as many frames as it needs.
    ///
    /// This waits for room in the queue to the helper, at most
    /// `audio_stall_timeout` (2 s by default); past that the helper is treated
    /// as hung and killed. Call it from a task, never from a real-time capture
    /// callback; there, use [`try_send_audio`](Self::try_send_audio).
    ///
    /// # Errors
    /// [`ClientError::StreamClosed`] after `finish`, `cancel` or a stream
    /// error; the helper's failure if it died.
    pub async fn send_audio(&self, pcm: &[i16]) -> Result<(), ClientError> {
        self.check_open()?;
        for chunk in pcm.chunks(MAX_SAMPLES_PER_FRAME) {
            let bytes = frame::encode_audio(self.id, chunk)
                .map_err(|e| ClientError::Protocol(e.to_string()))?;
            self.conn.write(bytes).await?;
        }
        Ok(())
    }

    /// Queue one frame's worth of PCM without waiting. Returns
    /// [`ClientError::Backpressure`] when the queue to the helper is full, and
    /// sends nothing; the caller decides whether to drop, buffer or retry.
    ///
    /// # Errors
    /// [`ClientError::Backpressure`]; [`ClientError::Protocol`] for more than
    /// [`MAX_SAMPLES_PER_FRAME`] samples; otherwise as
    /// [`send_audio`](Self::send_audio).
    pub fn try_send_audio(&self, pcm: &[i16]) -> Result<(), ClientError> {
        self.check_open()?;
        if pcm.is_empty() {
            return Ok(());
        }
        let bytes =
            frame::encode_audio(self.id, pcm).map_err(|e| ClientError::Protocol(e.to_string()))?;
        match self.conn.writer.try_send(bytes) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(ClientError::Backpressure),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(self.conn.dead_error()),
        }
    }

    /// No more audio: the engine flushes, then the stream yields its last
    /// results and [`StreamEvent::Closed`].
    ///
    /// # Errors
    /// As [`send_audio`](Self::send_audio).
    pub async fn finish(&mut self) -> Result<(), ClientError> {
        self.check_open()?;
        self.finished = true;
        let bytes = frame::encode_control(&Request::Finish { stream: self.id })
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        self.conn.write(bytes).await
    }

    /// Drop the stream now. Nothing more is yielded for it, and no other
    /// stream is affected.
    pub fn cancel(&mut self) {
        if self.cancelled {
            return;
        }
        self.cancelled = true;
        // Forget the route first, so nothing already in flight is delivered.
        let open = self.conn.unroute(self.id);
        let (_, closed) = mpsc::unbounded_channel();
        self.events = closed;
        if open && self.conn.is_alive() {
            self.conn.send_soon(Request::Cancel { stream: self.id });
        }
    }

    /// The next event, or `None` after a terminal one.
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        self.events.recv().await
    }

    fn check_open(&self) -> Result<(), ClientError> {
        if self.finished || self.cancelled {
            return Err(ClientError::StreamClosed);
        }
        let routes = lock(&self.conn.routes);
        if let Some(dead) = &routes.dead {
            return Err(dead.clone());
        }
        if routes.streams.contains_key(&self.id) {
            Ok(())
        } else {
            Err(ClientError::StreamClosed)
        }
    }
}

impl Drop for SpeechStream {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel();
        }
    }
}

// ------------------------------------------------------------------ connection

struct Conn {
    generation: u64,
    capabilities: Capabilities,
    writer: mpsc::Sender<Vec<u8>>,
    routes: Mutex<Routes>,
    next_id: AtomicU64,
    next_stream: AtomicU32,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    /// Set before a deliberate shutdown, so its exit is not a crash.
    closing: AtomicBool,
    /// How the process ended, once the reaper knows.
    exit_status: Mutex<Option<String>>,
    owner: Weak<Shared>,
    stall: Duration,
    heartbeat_timeout: Duration,
}

#[derive(Default)]
struct Routes {
    dead: Option<ClientError>,
    pending: HashMap<u64, Waiter>,
    streams: HashMap<u32, mpsc::UnboundedSender<StreamEvent>>,
}

struct Waiter {
    reply: oneshot::Sender<Result<Reply, ClientError>>,
    progress: Option<mpsc::UnboundedSender<Progress>>,
}

impl Conn {
    async fn spawn(
        config: &SpeechdConfig,
        generation: u64,
        owner: Weak<Shared>,
    ) -> Result<Arc<Self>, ClientError> {
        let mut child = Command::new(&config.program)
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ClientError::Spawn(format!("{}: {e}", config.program.display())))?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ClientError::Spawn("no pipes to the helper".into()));
        };
        let mut stdout = BufReader::new(stdout);
        let capabilities = match tokio::time::timeout(
            config.handshake_timeout,
            handshake(&mut stdin, &mut stdout, &config.client_name),
        )
        .await
        {
            Ok(Ok(capabilities)) => capabilities,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(ClientError::Timeout("hello")),
        };
        let (writer, queue) = mpsc::channel(WRITE_QUEUE);
        let (kill, killed) = oneshot::channel();
        let conn = Arc::new(Self {
            generation,
            capabilities,
            writer,
            routes: Mutex::new(Routes::default()),
            next_id: AtomicU64::new(1),
            next_stream: AtomicU32::new(1),
            kill: Mutex::new(Some(kill)),
            closing: AtomicBool::new(false),
            exit_status: Mutex::new(None),
            owner,
            stall: config.audio_stall_timeout,
            heartbeat_timeout: config.heartbeat_timeout,
        });
        tokio::spawn(write_loop(Arc::downgrade(&conn), stdin, queue));
        tokio::spawn(heartbeat(Arc::downgrade(&conn), config.heartbeat_interval));
        tokio::spawn(read_loop(conn.clone(), stdout));
        tokio::spawn(reap(Arc::downgrade(&conn), child, killed));
        Ok(conn)
    }

    fn is_alive(&self) -> bool {
        lock(&self.routes).dead.is_none()
    }

    fn dead_error(&self) -> ClientError {
        lock(&self.routes)
            .dead
            .clone()
            .unwrap_or_else(|| ClientError::HelperCrashed("connection closed".into()))
    }

    /// Fail everything waiting on this helper and kill it. Idempotent.
    fn fail(self: &Arc<Self>, error: ClientError) {
        let error = if self.closing.load(Ordering::SeqCst) {
            ClientError::ShutDown
        } else {
            error
        };
        let (pending, streams) = {
            let mut routes = lock(&self.routes);
            if routes.dead.is_some() {
                return;
            }
            routes.dead = Some(error.clone());
            (
                std::mem::take(&mut routes.pending),
                std::mem::take(&mut routes.streams),
            )
        };
        self.kill();
        for waiter in pending.into_values() {
            let _ = waiter.reply.send(Err(error.clone()));
        }
        let failure = StreamFailure {
            code: error.code(),
            message: error.to_string(),
        };
        for events in streams.into_values() {
            let _ = events.send(StreamEvent::Failed(failure.clone()));
        }
        if !self.closing.load(Ordering::SeqCst)
            && let Some(owner) = self.owner.upgrade()
        {
            owner.conn_died(self.generation);
        }
    }

    /// A deliberate end: no restart follows.
    fn close(self: &Arc<Self>, error: ClientError) {
        self.closing.store(true, Ordering::SeqCst);
        self.fail(error);
    }

    fn kill(&self) {
        if let Some(kill) = lock(&self.kill).take() {
            let _ = kill.send(());
        }
    }

    fn route(&self, stream: u32) -> Result<mpsc::UnboundedReceiver<StreamEvent>, ClientError> {
        let mut routes = lock(&self.routes);
        if let Some(dead) = &routes.dead {
            return Err(dead.clone());
        }
        let (events, receiver) = mpsc::unbounded_channel();
        routes.streams.insert(stream, events);
        Ok(receiver)
    }

    /// Forget a stream. Returns whether it was still open.
    fn unroute(&self, stream: u32) -> bool {
        lock(&self.routes).streams.remove(&stream).is_some()
    }

    fn unexpected(self: &Arc<Self>, reply: &Reply) -> ClientError {
        let error = ClientError::Protocol(format!("unexpected reply {reply:?}"));
        self.fail(error.clone());
        error
    }

    /// Queue a frame, waiting at most the stall timeout for room.
    async fn write(self: &Arc<Self>, bytes: Vec<u8>) -> Result<(), ClientError> {
        if let Some(dead) = &lock(&self.routes).dead {
            return Err(dead.clone());
        }
        match tokio::time::timeout(self.stall, self.writer.send(bytes)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(self.dead_error()),
            Err(_) => {
                let error = ClientError::HelperUnresponsive("stopped reading its input".into());
                self.fail(error.clone());
                Err(error)
            }
        }
    }

    /// Queue a control message without waiting, for paths that cannot await.
    fn send_soon(self: &Arc<Self>, request: Request) {
        let Ok(bytes) = frame::encode_control(&request) else {
            return;
        };
        if let Err(mpsc::error::TrySendError::Full(bytes)) = self.writer.try_send(bytes)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let conn = self.clone();
            runtime.spawn(async move {
                let _ = conn.write(bytes).await;
            });
        }
    }

    async fn call(
        self: &Arc<Self>,
        build: impl FnOnce(u64) -> Request,
        timeout: Duration,
        progress: Option<mpsc::UnboundedSender<Progress>>,
        what: &'static str,
    ) -> Result<Reply, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let bytes =
            frame::encode_control(&build(id)).map_err(|e| ClientError::Protocol(e.to_string()))?;
        let (reply, answer) = oneshot::channel();
        {
            let mut routes = lock(&self.routes);
            if let Some(dead) = &routes.dead {
                return Err(dead.clone());
            }
            routes.pending.insert(id, Waiter { reply, progress });
        }
        self.write(bytes).await?;
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.dead_error()),
            Err(_) => {
                lock(&self.routes).pending.remove(&id);
                self.fail(ClientError::HelperUnresponsive(format!(
                    "no answer to {what}"
                )));
                Err(ClientError::Timeout(what))
            }
        }
    }

    async fn load(
        self: &Arc<Self>,
        engine: &str,
        timeout: Duration,
        progress: Option<mpsc::UnboundedSender<Progress>>,
    ) -> Result<Loaded, ClientError> {
        let engine = engine.to_owned();
        match self
            .call(
                move |id| Request::Load { id, engine },
                timeout,
                progress,
                "load",
            )
            .await?
        {
            Reply::Loaded {
                model,
                revision,
                elapsed_ms,
                ..
            } => Ok(Loaded {
                model,
                revision,
                elapsed: Duration::from_millis(elapsed_ms),
            }),
            other => Err(self.unexpected(&other)),
        }
    }

    async fn warmup(
        self: &Arc<Self>,
        engine: &str,
        timeout: Duration,
        progress: Option<mpsc::UnboundedSender<Progress>>,
    ) -> Result<Duration, ClientError> {
        let engine = engine.to_owned();
        match self
            .call(
                move |id| Request::Warmup { id, engine },
                timeout,
                progress,
                "warmup",
            )
            .await?
        {
            Reply::Warmed { elapsed_ms, .. } => Ok(Duration::from_millis(elapsed_ms)),
            other => Err(self.unexpected(&other)),
        }
    }

    /// Route one reply. An `Err` is fatal to the connection.
    fn dispatch(self: &Arc<Self>, reply: Reply) -> Result<(), ClientError> {
        match reply {
            Reply::Progress {
                id,
                phase,
                bytes,
                total,
                ..
            } => {
                if let Some(Waiter {
                    progress: Some(progress),
                    ..
                }) = lock(&self.routes).pending.get(&id)
                {
                    let _ = progress.send(Progress {
                        phase,
                        bytes,
                        total,
                    });
                }
            }
            Reply::Error {
                id: Some(id),
                code,
                message,
                ..
            } => self.answer(id, Err(ClientError::Remote { code, message })),
            Reply::Error {
                stream: Some(stream),
                code,
                message,
                ..
            } => self.end_stream(stream, StreamEvent::Failed(StreamFailure { code, message })),
            Reply::Error { code, message, .. } => {
                return Err(if code == ErrorCode::Protocol {
                    ClientError::Protocol(message)
                } else {
                    ClientError::Remote { code, message }
                });
            }
            Reply::Partial {
                stream,
                text,
                start_ms,
                end_ms,
                utterance,
            } => self.to_stream(
                stream,
                StreamEvent::Partial {
                    text,
                    start_ms,
                    end_ms,
                    utterance,
                },
            ),
            Reply::Final(text) => self.to_stream(text.stream, StreamEvent::Final(text)),
            Reply::Vad {
                stream,
                event,
                sample,
                probability,
            } => self.to_stream(
                stream,
                StreamEvent::Vad {
                    event,
                    sample,
                    probability,
                },
            ),
            Reply::Closed { stream, reason } => {
                if reason == CloseReason::Finished
                    && let Some(owner) = self.owner.upgrade()
                {
                    // Before the event, so a caller that sees Closed sees the
                    // reset budget too.
                    owner.stream_succeeded();
                }
                self.end_stream(stream, StreamEvent::Closed(reason));
            }
            Reply::Hello { .. } => return Err(ClientError::Protocol("a second hello".into())),
            Reply::Loaded { id, .. }
            | Reply::Warmed { id, .. }
            | Reply::Unloaded { id, .. }
            | Reply::StreamOpened { id, .. }
            | Reply::Health { id, .. }
            | Reply::Installed { id, .. }
            | Reply::Bye { id } => self.answer(id, Ok(reply)),
        }
        Ok(())
    }

    fn answer(&self, id: u64, result: Result<Reply, ClientError>) {
        // An answer to a request that already timed out has nobody to go to.
        if let Some(waiter) = lock(&self.routes).pending.remove(&id) {
            let _ = waiter.reply.send(result);
        }
    }

    /// Events for a stream the caller cancelled are dropped here.
    fn to_stream(&self, stream: u32, event: StreamEvent) {
        if let Some(events) = lock(&self.routes).streams.get(&stream) {
            let _ = events.send(event);
        }
    }

    fn end_stream(&self, stream: u32, event: StreamEvent) {
        if let Some(events) = lock(&self.routes).streams.remove(&stream) {
            let _ = events.send(event);
        }
    }
}

async fn handshake(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    client: &str,
) -> Result<Capabilities, ClientError> {
    let hello = frame::encode_control(&Request::Hello {
        id: 0,
        protocol: PROTOCOL_VERSION,
        client: client.to_owned(),
    })
    .map_err(|e| ClientError::Protocol(e.to_string()))?;
    let crashed = |e: std::io::Error| ClientError::HelperCrashed(format!("during hello: {e}"));
    stdin.write_all(&hello).await.map_err(crashed)?;
    stdin.flush().await.map_err(crashed)?;
    let json = match frame::read_frame_async(stdout).await {
        Ok(Some(Frame::Control(json))) => json,
        Ok(Some(Frame::Audio { .. })) => {
            return Err(ClientError::Protocol("audio before hello".into()));
        }
        Ok(None) => return Err(ClientError::HelperCrashed("exited before hello".into())),
        Err(e) => return Err(frame_failure(e)),
    };
    match serde_json::from_slice::<Reply>(&json)
        .map_err(|e| ClientError::Protocol(e.to_string()))?
    {
        Reply::Hello {
            protocol: PROTOCOL_VERSION,
            capabilities,
            ..
        } => {
            let mut capabilities: Capabilities = serde_json::from_value(capabilities)
                .map_err(|e| ClientError::Protocol(format!("capabilities: {e}")))?;
            // A newer helper's engine of a kind this version cannot drive.
            capabilities
                .engines
                .retain(|engine| engine.kind != EngineKind::Unknown);
            Ok(capabilities)
        }
        Reply::Hello { protocol, .. } => Err(ClientError::VersionMismatch {
            client: PROTOCOL_VERSION,
            helper: Some(protocol),
            supported: Vec::new(),
        }),
        Reply::Error {
            code: ErrorCode::VersionMismatch,
            supported,
            ..
        } => Err(ClientError::VersionMismatch {
            client: PROTOCOL_VERSION,
            helper: None,
            supported,
        }),
        other => Err(ClientError::Protocol(format!(
            "expected hello, got {other:?}"
        ))),
    }
}

/// A frame that could not be read: torn by a dying helper, or malformed.
fn frame_failure(error: FrameError) -> ClientError {
    match error {
        FrameError::Truncated | FrameError::Io(_) => ClientError::HelperCrashed(error.to_string()),
        other => ClientError::Protocol(other.to_string()),
    }
}

async fn write_loop(conn: Weak<Conn>, mut stdin: ChildStdin, mut queue: mpsc::Receiver<Vec<u8>>) {
    while let Some(bytes) = queue.recv().await {
        if let Err(e) = async {
            stdin.write_all(&bytes).await?;
            stdin.flush().await
        }
        .await
        {
            // Usually the helper has exited: let the reader take what it wrote
            // first, and fail only if nothing else does.
            fail_after_drain(
                &conn,
                ClientError::HelperCrashed(format!("writing to it: {e}")),
            )
            .await;
            return;
        }
    }
    // Every sender is gone: EOF tells the helper to exit.
}

async fn read_loop(conn: Arc<Conn>, mut stdout: BufReader<ChildStdout>) {
    let error = loop {
        match frame::read_frame_async(&mut stdout).await {
            Ok(Some(Frame::Control(json))) => match serde_json::from_slice::<Reply>(&json) {
                Ok(reply) => {
                    if let Err(e) = conn.dispatch(reply) {
                        break e;
                    }
                }
                Err(e) => match unknown_type(&json) {
                    // From a newer helper: skip it, or fail just its request.
                    Some(None) => {}
                    Some(Some(id)) => conn.answer(
                        id,
                        Err(ClientError::Protocol("a reply of an unknown type".into())),
                    ),
                    None => break ClientError::Protocol(format!("malformed reply: {e}")),
                },
            },
            Ok(Some(Frame::Audio { .. })) => {
                break ClientError::Protocol("the helper sent audio".into());
            }
            Ok(None) => {
                let status = lock(&conn.exit_status).clone();
                break ClientError::HelperCrashed(
                    status.unwrap_or_else(|| "it closed its output".into()),
                );
            }
            Err(e) => break frame_failure(e),
        }
    };
    conn.fail(error);
}

async fn heartbeat(conn: Weak<Conn>, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        let Some(conn) = conn.upgrade() else { return };
        let streams_open = {
            let routes = lock(&conn.routes);
            if routes.dead.is_some() {
                return;
            }
            !routes.streams.is_empty()
        };
        if streams_open {
            // A missed deadline fails the connection inside `call`.
            let _ = conn
                .call(
                    |id| Request::Health { id },
                    conn.heartbeat_timeout,
                    None,
                    "a heartbeat",
                )
                .await;
        }
    }
}

async fn reap(conn: Weak<Conn>, mut child: Child, killed: oneshot::Receiver<()>) {
    tokio::select! {
        status = child.wait() => {
            let detail = status.map_or_else(|e| e.to_string(), |s| s.to_string());
            if let Some(conn) = conn.upgrade() {
                *lock(&conn.exit_status) = Some(detail.clone());
            }
            // The reader fails the connection at EOF once it has read what the
            // helper wrote before exiting. This catches a stdout that never
            // closes (a grandchild holding it, say).
            fail_after_drain(&conn, ClientError::HelperCrashed(detail)).await;
        }
        _ = killed => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

/// Fail the connection unless something else does within [`DRAIN_AFTER_EXIT`].
async fn fail_after_drain(conn: &Weak<Conn>, error: ClientError) {
    let deadline = Instant::now() + DRAIN_AFTER_EXIT;
    while Instant::now() < deadline {
        match conn.upgrade() {
            Some(conn) if conn.is_alive() => {}
            _ => return,
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if let Some(conn) = conn.upgrade() {
        conn.fail(error);
    }
}

/// For a reply that did not parse: `Some(id)` if it is an object whose `type`
/// this version does not know (with its `id`, if any), `None` if it is
/// malformed in a way no version allows.
fn unknown_type(json: &[u8]) -> Option<Option<u64>> {
    const KNOWN: [&str; 14] = [
        "hello",
        "loaded",
        "warmed",
        "unloaded",
        "stream_opened",
        "health",
        "progress",
        "installed",
        "bye",
        "partial",
        "final",
        "vad",
        "error",
        "closed",
    ];
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let kind = value.get("type")?.as_str()?;
    if KNOWN.contains(&kind) {
        return None;
    }
    Some(value.get("id").and_then(serde_json::Value::as_u64))
}
