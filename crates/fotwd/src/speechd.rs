//! Parakeet Unified, the default local meeting engine (#244), served by the
//! `fotw-speechd` helper (#243).
//!
//! This is the daemon's side of it: where the helper and its model store are,
//! preparing the engine before a recording, and the one explicit action that
//! may download it. The engine itself is [`fotw_stt::SpeechdEngine`].
//!
//! - **Selection.** A saved transcription choice names it by
//!   [`ENGINE`] in place of a ggml path (see [`crate::stt::SttSettings`]).
//! - **Preparation.** Start spawns the helper, loads the model and warms it up
//!   before capture opens. A model that is not installed refuses Start with
//!   the command that installs it; nothing downloads during capture.
//! - **Install.** `fotwd stt models install parakeet-unified` and the
//!   Transcription settings send the helper its `install` request, the only
//!   request that may touch the network. `import parakeet-unified <dir>` sends
//!   `install {from}`, which opens no socket.
//!
//! **One helper per daemon.** [`helper`] hands out the shared client, spawned
//! lazily on first use and kept across recordings, whichever engine the
//! meeting's final transcript uses: Parakeet borrows it, and so do live
//! captions (#245) beside a Whisper meeting. Each engine loads and warms its
//! own model at preparation; the client replays those after a restart. A
//! client spent by its restart budget, or shut down, is replaced on the next
//! call. Its tasks belong to the runtime that started it, so only the daemon's
//! own runtime may call [`helper`]; a throwaway runtime (a settings check, an
//! install) starts a private helper instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fotw_speechd_client::message::ProgressPhase;
use fotw_speechd_client::{SpeechdClient, SpeechdConfig};
use fotw_stt::SpeechdEngine;

pub use fotw_stt::speechd::{
    PARAKEET_UNIFIED as ENGINE, PARAKEET_UNIFIED_LICENSE as LICENSE,
    PARAKEET_UNIFIED_REPOSITORY as REPOSITORY, PARAKEET_UNIFIED_REVISION as REVISION,
};

/// Bytes the pinned files add up to (the helper's pin list, which leaves out
/// the repository's README).
pub const BYTES: u64 = 614_080_920;

/// Parakeet's model card, which states the license, at the pinned revision.
#[must_use]
pub fn license_url() -> String {
    PARAKEET.license_url()
}

/// A model the helper serves and `fotwd stt models` can install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperModel {
    /// Model id, as the helper and the CLI name it.
    pub id: &'static str,
    /// Hugging Face repository.
    pub repository: &'static str,
    /// The pinned revision.
    pub revision: &'static str,
    /// Bytes the pinned files add up to.
    pub bytes: u64,
    /// SPDX license of the weights.
    pub license: &'static str,
    /// One line on what it is.
    pub note: &'static str,
}

impl HelperModel {
    /// The model card, which states the license, at the pinned revision.
    #[must_use]
    pub fn license_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/blob/{}/README.md",
            self.repository, self.revision
        )
    }
}

/// Parakeet Unified, the default on-device meeting model.
pub const PARAKEET: HelperModel = HelperModel {
    id: ENGINE,
    repository: REPOSITORY,
    revision: REVISION,
    bytes: BYTES,
    license: LICENSE,
    note: "Parakeet Unified 0.6B, English; the default on-device model, run by the speech helper",
};

/// Parakeet realtime EOU 120M, 320 ms: live captions in local mode (#245).
/// Display only; the meeting's text still comes from the local engine.
pub const CAPTIONS: HelperModel = HelperModel {
    id: crate::captions::ENGINE,
    repository: "FluidInference/parakeet-realtime-eou-120m-coreml",
    revision: "40a23f4c0b333aa17ad8c0f2ea47ec2347f2f355",
    bytes: 224_238_270,
    license: "LicenseRef-NVIDIA-Open-Model-License",
    note: "Parakeet realtime EOU 120M; live captions while people talk (display only)",
};

/// Every model installed through the helper.
pub const HELPER_MODELS: &[HelperModel] = &[PARAKEET, CAPTIONS];

/// A helper model by id.
#[must_use]
pub fn helper_model(id: &str) -> Option<&'static HelperModel> {
    HELPER_MODELS.iter().find(|m| m.id == id.trim())
}

/// Whether `model`'s pinned revision is in the store (unchecked, as
/// [`present`]).
#[must_use]
pub fn present_model(data_root: &Path, model: &HelperModel) -> bool {
    models_root(data_root)
        .join(model.id)
        .join(model.revision)
        .is_dir()
}

/// Whether a saved model choice names Parakeet Unified.
#[must_use]
pub fn is_engine(model: &str) -> bool {
    model.trim() == ENGINE
}

/// The helper's model store: beside the Whisper files, in a folder of its own.
#[must_use]
pub fn models_root(data_root: &Path) -> PathBuf {
    crate::stt::models_dir(data_root).join("speechd")
}

/// Whether the pinned revision is in the store, by the layout the protocol
/// fixes (`<models-root>/<model>/<revision>/`). Unchecked: the helper checks
/// every file's size when it loads.
#[must_use]
pub fn present(data_root: &Path) -> bool {
    present_model(data_root, &PARAKEET)
}

/// How to start the helper.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The executable.
    pub program: PathBuf,
    /// Arguments after `serve --models-root <dir>`; empty in production.
    pub extra_args: Vec<OsString>,
}

impl Launch {
    /// The helper bundled with the app (`Contents/Helpers/fotw-speechd`), or
    /// `FOTW_SPEECHD_PATH`.
    ///
    /// # Errors
    ///
    /// When this build has no helper.
    pub fn bundled() -> Result<Self, String> {
        Self::bundled_from(fotw_speechd_client::locate_helper())
    }

    /// [`bundled`](Self::bundled), with the lookup's answer passed in.
    ///
    /// # Errors
    ///
    /// When `helper` is `None`.
    pub fn bundled_from(helper: Option<PathBuf>) -> Result<Self, String> {
        helper
            .map(|program| Self {
                program,
                extra_args: Vec::new(),
            })
            .ok_or_else(|| {
                "Parakeet Unified runs in the speech helper (fotw-speechd), which this build of \
                 FlyOnTheWall does not include. Choose a Whisper model in Transcription settings, \
                 or use a build with the helper."
                    .to_owned()
            })
    }

    fn config(&self, data_root: &Path) -> SpeechdConfig {
        let mut config = SpeechdConfig::new(&self.program, models_root(data_root));
        config.client_name = concat!("fotwd/", env!("CARGO_PKG_VERSION")).to_owned();
        for arg in &self.extra_args {
            config = config.arg(arg.clone());
        }
        config
    }
}

type Registry = std::collections::HashMap<(PathBuf, PathBuf), SpeechdClient>;

/// The daemon's helpers, one per helper program and data root.
fn registry() -> &'static tokio::sync::Mutex<Registry> {
    static HELPERS: std::sync::OnceLock<tokio::sync::Mutex<Registry>> = std::sync::OnceLock::new();
    HELPERS.get_or_init(Default::default)
}

/// The daemon's shared helper for this library, started on first use.
///
/// # Errors
///
/// When this build has no helper, or it does not start.
pub async fn helper(data_root: &Path) -> Result<SpeechdClient, String> {
    helper_with(&Launch::bundled()?, data_root).await
}

/// [`helper`] with the program passed in.
///
/// # Errors
///
/// When the helper does not start.
pub async fn helper_with(launch: &Launch, data_root: &Path) -> Result<SpeechdClient, String> {
    let key = (launch.program.clone(), data_root.to_owned());
    let mut helpers = registry().lock().await;
    if let Some(client) = helpers.get(&key)
        && !client.status().failed
    {
        return Ok(client.clone());
    }
    let client = start(launch, data_root).await?;
    helpers.insert(key, client.clone());
    Ok(client)
}

async fn start(launch: &Launch, data_root: &Path) -> Result<SpeechdClient, String> {
    SpeechdClient::start(launch.config(data_root))
        .await
        .map_err(|e| format!("Could not start the speech helper: {e}"))
}

/// Load Parakeet Unified in the daemon's shared helper and warm it up, ready
/// for a recording. Returns the engine and the revision to record as
/// provenance.
///
/// # Errors
///
/// When the model is not installed (with how to install it), or the helper
/// does not start, load or answer.
pub async fn prepare(
    launch: &Launch,
    data_root: &Path,
) -> Result<(Arc<SpeechdEngine>, String), String> {
    load(helper_with(launch, data_root).await?).await
}

/// [`prepare`] in a helper of its own that exits with the engine: for a
/// runtime that does not outlive the call, such as a settings check.
///
/// # Errors
///
/// As [`prepare`].
pub async fn prepare_private(
    launch: &Launch,
    data_root: &Path,
) -> Result<(Arc<SpeechdEngine>, String), String> {
    load(start(launch, data_root).await?).await
}

async fn load(client: SpeechdClient) -> Result<(Arc<SpeechdEngine>, String), String> {
    let engine = SpeechdEngine::prepare(client, ENGINE)
        .await
        .map_err(|e| match e.detail {
            Some(detail) if e.class != fotw_stt::SttErrorClass::Unsupported => {
                format!("{} ({detail})", e.message)
            }
            _ => e.message,
        })?;
    let revision = engine.revision().to_owned();
    Ok((Arc::new(engine), revision))
}

/// Install progress, in the helper's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    /// Downloading or checking.
    pub phase: ProgressPhase,
    /// Bytes done.
    pub bytes: u64,
    /// Bytes in all.
    pub total: u64,
}

/// Explicit setup: have the helper download Parakeet Unified at its pinned
/// revision, or import it from `from` (a copy of the repository's files)
/// without opening a socket. Blocks; run it on a worker. Every file is
/// checked against its pin before the revision is selected, and a failure
/// leaves any previous install as it was.
///
/// `cancelled` is polled while the helper works; when it turns true the
/// helper is stopped and nothing is selected.
///
/// # Errors
///
/// When the helper is missing or refuses, or setup is cancelled.
pub fn install(
    launch: &Launch,
    data_root: &Path,
    from: Option<&Path>,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(InstallProgress),
) -> Result<String, String> {
    install_model(launch, data_root, &PARAKEET, from, cancelled, progress)
}

/// [`install`] for any [`HELPER_MODELS`] entry, in a private helper.
///
/// # Errors
///
/// As [`install`].
pub fn install_model(
    launch: &Launch,
    data_root: &Path,
    model: &HelperModel,
    from: Option<&Path>,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(InstallProgress),
) -> Result<String, String> {
    let root = models_root(data_root);
    crate::model_setup::private_dir(&root)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let client = start(launch, data_root).await?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let install = client.install(model.id, from, Some(tx));
        tokio::pin!(install);
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
        let result = loop {
            tokio::select! {
                result = &mut install => break result,
                Some(p) = rx.recv() => progress(InstallProgress {
                    phase: p.phase,
                    bytes: p.bytes.unwrap_or(0),
                    total: p.total.unwrap_or(model.bytes),
                }),
                _ = ticker.tick() => if cancelled() {
                    client.shutdown().await;
                    return Err("Model setup cancelled; previous model preserved".to_owned());
                },
            }
        };
        // The client forwards a progress event before the answer that follows
        // it, so it is already queued; select! may simply have taken the
        // answer first.
        while let Ok(p) = rx.try_recv() {
            progress(InstallProgress {
                phase: p.phase,
                bytes: p.bytes.unwrap_or(0),
                total: p.total.unwrap_or(model.bytes),
            });
        }
        client.shutdown().await;
        let revision = result.map_err(|e| format!("{} was not installed: {e}", model.id))?;
        if revision != model.revision {
            return Err(format!(
                "the speech helper installed {} at {revision}, but this build expects {}",
                model.id, model.revision
            ));
        }
        Ok(revision)
    })
}

#[cfg(test)]
pub(crate) mod testing {
    //! The model-free helper for the daemon's tests.
    use super::*;

    /// `target/<profile>/fotw-speechd-fake`, built on first use when this
    /// test binary was built on its own.
    pub(crate) fn fake_helper() -> PathBuf {
        static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        PATH.get_or_init(|| {
            let exe = std::env::current_exe().unwrap();
            let path = exe
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("fotw-speechd-fake");
            if !path.is_file() {
                let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
                let status = std::process::Command::new(cargo)
                    .args([
                        "build",
                        "-p",
                        "fotw-speechd-client",
                        "--bin",
                        "fotw-speechd-fake",
                    ])
                    .status()
                    .unwrap();
                assert!(status.success(), "could not build fotw-speechd-fake");
            }
            path
        })
        .clone()
    }

    /// The fake, serving `parakeet-unified` at the pinned revision; installed
    /// when `<models_root>/parakeet-unified/<revision>` exists.
    pub(crate) fn fake_launch() -> Launch {
        Launch {
            program: fake_helper(),
            extra_args: vec![
                "--fake-engines".into(),
                "--pinned".into(),
                format!("{ENGINE}@{REVISION}").into(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::fake_launch;
    use super::*;

    /// The fake answers `install` right behind its `verify` progress, so the
    /// answer and the progress are often ready together. Every install must
    /// still report the progress the helper sent; one try in ten or twenty
    /// used to lose it, so this repeats enough to catch that.
    #[test]
    fn install_reports_progress_the_helper_sent_just_before_answering() {
        for attempt in 0..128 {
            let root = tempfile::tempdir().unwrap();
            let mut seen = Vec::new();
            install(&fake_launch(), root.path(), None, &|| false, &mut |p| {
                seen.push(p.phase);
            })
            .unwrap();
            assert!(
                seen.contains(&ProgressPhase::Verify),
                "attempt {attempt}: {seen:?}"
            );
        }
    }

    #[test]
    fn install_selects_the_pinned_revision_and_prepare_then_loads_it() {
        let root = tempfile::tempdir().unwrap();
        assert!(!present(root.path()));
        let mut seen = Vec::new();
        let revision = install(&fake_launch(), root.path(), None, &|| false, &mut |p| {
            seen.push(p.phase);
        })
        .unwrap();
        assert_eq!(revision, REVISION);
        assert!(present(root.path()));
        assert!(seen.contains(&ProgressPhase::Verify), "{seen:?}");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (engine, revision) = runtime
            .block_on(prepare(&fake_launch(), root.path()))
            .unwrap();
        assert_eq!(revision, REVISION);
        use fotw_stt::RecognizerFactory;
        assert_eq!(engine.model(), ENGINE);
        assert_eq!(engine.provider(), fotw_stt::speechd::PROVIDER);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_helper_serves_every_engine_and_outlives_a_prepared_engine() {
        let root = tempfile::tempdir().unwrap();
        let first = helper_with(&fake_launch(), root.path()).await.unwrap();
        let generation = first.status().generation;
        // Captions borrow it beside a Whisper meeting; nothing else holds it.
        first.load("fake-stream").await.unwrap();
        drop(first);
        let again = helper_with(&fake_launch(), root.path()).await.unwrap();
        assert_eq!(again.status().generation, generation, "same helper process");
        assert!(
            again
                .health()
                .await
                .unwrap()
                .loaded
                .contains(&"fake-stream".to_owned())
        );
        // A spent client is replaced rather than handed out.
        again.shutdown().await;
        let fresh = helper_with(&fake_launch(), root.path()).await.unwrap();
        assert!(fresh.health().await.is_ok());
    }

    #[test]
    fn the_license_is_shown_from_the_pinned_model_card() {
        assert_eq!(LICENSE, "CC-BY-4.0");
        assert!(license_url().starts_with("https://huggingface.co/FluidInference/"));
        assert!(license_url().contains(REVISION));
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::stt::{SETTINGS_KEY, SttEngine, SttSettings};
    use fotw_store::{Db, DbKey};

    #[test]
    fn a_new_library_defaults_local_transcription_to_parakeet_and_the_choice_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let key = DbKey::from_bytes([0x51; 32]);
        {
            let mut db = Db::open(&path, &key).unwrap();
            let fresh = SttSettings::read(&db).unwrap();
            assert_eq!(
                fresh.engine,
                SttEngine::Deepgram,
                "the engine default is unchanged"
            );
            assert!(is_engine(&fresh.model), "local default: {fresh:?}");
            SttSettings::for_selection(&db, SttEngine::Local)
                .write(&mut db)
                .unwrap();
        }
        // A restart is a fresh handle on the same library.
        let db = Db::open(&path, &key).unwrap();
        let saved = SttSettings::read(&db).unwrap();
        assert_eq!(saved.engine, SttEngine::Local);
        assert_eq!(saved.model, ENGINE);
        assert!(
            db.get_setting(SETTINGS_KEY)
                .unwrap()
                .unwrap()
                .contains(r#""model":"parakeet-unified""#)
        );
    }

    #[test]
    fn repairing_a_corrupt_choice_keeps_the_whisper_default_it_had() {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x53; 32])).unwrap();
        db.put_setting(SETTINGS_KEY, "{").unwrap();
        let repaired = SttSettings::for_selection(&db, SttEngine::Local);
        assert_eq!(repaired.engine, SttEngine::Local);
        assert!(!is_engine(&repaired.model), "{repaired:?}");
        assert_eq!(
            repaired.model_path(Path::new("/data")),
            Path::new("/data/models/ggml-large-v3-turbo-q5_0.bin")
        );
    }

    #[test]
    fn an_existing_saved_whisper_choice_is_left_as_it_was() {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x52; 32])).unwrap();
        let data_root = Path::new("/data");
        for (row, expected) in [
            (
                r#"{"engine":"local"}"#,
                "/data/models/ggml-large-v3-turbo-q5_0.bin",
            ),
            (
                r#"{"engine":"local","model":"","language":"en"}"#,
                "/data/models/ggml-large-v3-turbo-q5_0.bin",
            ),
            (
                r#"{"engine":"fallback","model":"/m/ggml-small.en.bin"}"#,
                "/m/ggml-small.en.bin",
            ),
        ] {
            db.put_setting(SETTINGS_KEY, row).unwrap();
            for settings in [
                SttSettings::read(&db).unwrap(),
                SttSettings::for_selection(&db, SttEngine::Local),
            ] {
                assert!(!is_engine(&settings.model), "{row}: {settings:?}");
                assert_eq!(settings.model_path(data_root), Path::new(expected), "{row}");
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod start_tests {
    use super::testing::fake_launch;
    use super::*;
    use crate::session::{SessionControl, Transcription};
    use crate::stt::{SttEngine, SttSettings, fixture_detector, local_transcription_with};

    fn parakeet() -> SttSettings {
        SttSettings {
            engine: SttEngine::Local,
            ..SttSettings::default()
        }
    }

    async fn start(
        data_root: &Path,
        launch: Result<Launch, String>,
    ) -> Result<Transcription, String> {
        let detector = fixture_detector(data_root);
        local_transcription_with(
            &parakeet(),
            data_root,
            "fixture",
            true,
            true,
            detector,
            move || launch,
        )
        .prepare(&SessionControl::new())
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_parakeet_model_refuses_start_with_the_install_hint() {
        let root = tempfile::tempdir().unwrap();
        let refused = start(root.path(), Ok(fake_launch()))
            .await
            .expect_err("Start must refuse rather than record without its model");
        assert!(
            refused.contains("fotwd stt models install parakeet-unified"),
            "{refused}"
        );
        assert!(refused.contains("Transcription settings"), "{refused}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_prepares_parakeet_behind_the_speech_gate_with_the_loaded_revision() {
        let root = tempfile::tempdir().unwrap();
        let data_root = root.path().to_owned();
        tokio::task::spawn_blocking(move || {
            install(&fake_launch(), &data_root, None, &|| false, &mut |_| {})
        })
        .await
        .unwrap()
        .unwrap();
        let Transcription::Local(legs) = start(root.path(), Ok(fake_launch())).await.unwrap()
        else {
            panic!("expected local legs");
        };
        assert_eq!(legs.revision.as_deref(), Some(REVISION));
        assert_eq!(legs.engine.provider(), fotw_stt::speechd::PROVIDER);
        assert_eq!(legs.engine.model(), ENGINE);
        assert_eq!(legs.engine.language(), Some("en"));
        // The fixture detector is not a loadable Silero model, so a leg that
        // must pass its audio through the gate cannot start: Parakeet never
        // sees ungated audio.
        let Err(gated) = legs.engine.create() else {
            panic!("each leg loads its detector");
        };
        assert!(gated.message.contains("speech detector"), "{gated:?}");
    }

    /// A local Whisper meeting's legs, with `live_captions` as given.
    async fn whisper_legs(
        data_root: &Path,
        live_captions: bool,
        launch: Result<Launch, String>,
    ) -> crate::session::LocalLegs {
        let settings = SttSettings {
            engine: SttEngine::Local,
            model: data_root
                .join("ggml-missing.bin")
                .to_string_lossy()
                .into_owned(),
            live_captions,
            ..SttSettings::default()
        };
        let detector = fixture_detector(data_root);
        let prepared = local_transcription_with(
            &settings,
            data_root,
            "fixture",
            true,
            true,
            detector,
            move || launch,
        )
        .prepare(&SessionControl::new())
        .await
        .expect("a Whisper meeting starts, model or not");
        let Transcription::Local(legs) = prepared else {
            panic!("expected local legs");
        };
        legs
    }

    fn install_caption_model(data_root: &Path) {
        std::fs::create_dir_all(
            models_root(data_root)
                .join(CAPTIONS.id)
                .join(CAPTIONS.revision),
        )
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_captions_join_a_local_meeting_only_when_on_and_installed() {
        // #245: on by default, but nothing happens until the caption model is
        // installed, and switching captions off keeps them off.
        let root = tempfile::tempdir().unwrap();
        let legs = whisper_legs(root.path(), true, Ok(fake_launch())).await;
        assert!(
            legs.captions.is_none(),
            "no caption model installed: no captions"
        );

        install_caption_model(root.path());
        let legs = whisper_legs(root.path(), true, Ok(fake_launch())).await;
        let captions = legs.captions.expect("installed and on: captions");
        assert_eq!(captions.model(), crate::captions::ENGINE);

        let legs = whisper_legs(root.path(), false, Ok(fake_launch())).await;
        assert!(legs.captions.is_none(), "switched off: no captions");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_captions_never_start_the_helper_at_start_and_need_one_in_the_build() {
        let root = tempfile::tempdir().unwrap();
        install_caption_model(root.path());
        // A build without the helper still starts the meeting, without captions.
        let legs = whisper_legs(root.path(), true, Launch::bundled_from(None)).await;
        assert!(legs.captions.is_none());
        // With one, preparing the meeting spawns nothing: the helper starts on
        // the first caption stream, after capture has begun.
        let before = pids_of_fake();
        let legs = whisper_legs(root.path(), true, Ok(fake_launch())).await;
        assert!(legs.captions.is_some());
        assert_eq!(
            pids_of_fake(),
            before,
            "no helper spawned to prepare captions"
        );
    }

    /// Running fake helpers, by pid.
    fn pids_of_fake() -> Vec<String> {
        let out = std::process::Command::new("pgrep")
            .args(["-f", "fotw-speechd-fake"])
            .output()
            .unwrap();
        let mut pids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        pids.sort();
        pids
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_build_without_the_helper_refuses_start_and_says_so() {
        let root = tempfile::tempdir().unwrap();
        let refused = start(root.path(), Launch::bundled_from(None))
            .await
            .expect_err("no helper, no Parakeet");
        assert!(refused.contains("fotw-speechd"), "{refused}");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod fallback_tests {
    use super::testing::fake_launch;
    use crate::session::SessionControl;
    use crate::stt::{SttEngine, SttSettings, fallback_transcription_with, fixture_detector};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_offline_backup_on_a_missing_parakeet_refuses_before_reading_the_cloud_key() {
        let root = tempfile::tempdir().unwrap();
        let detector = fixture_detector(root.path());
        let settings = SttSettings {
            engine: SttEngine::Fallback,
            ..SttSettings::default()
        };
        let refused = fallback_transcription_with(
            settings,
            root.path().to_owned(),
            Box::new(|| panic!("the backup is prepared before any cloud credential is read")),
            detector,
            || Ok(fake_launch()),
        )
        .prepare(&SessionControl::new())
        .await
        .expect_err("no backup model, no Start");
        assert!(
            refused.contains("fotwd stt models install parakeet-unified"),
            "{refused}"
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod recovery_tests {
    use super::*;
    use crate::failover::coverage::{EngineDescriptor, LocalPlan};

    #[test]
    fn recovering_saved_parakeet_audio_is_refused_plainly_and_the_audio_kept() {
        let root = tempfile::tempdir().unwrap();
        let plan = LocalPlan {
            engine: EngineDescriptor {
                provider: fotw_stt::speechd::PROVIDER.into(),
                model: ENGINE.into(),
                revision: Some(REVISION.into()),
                capabilities: fotw_stt::SttCapabilities::deepgram(),
            },
            language: Some("en".into()),
            endpoint: Default::default(),
        };
        let Err(refused) = crate::stt::saved_local_model(&plan, root.path(), root.path()) else {
            panic!("Parakeet recovery is not supported yet");
        };
        assert_eq!(
            refused,
            "Parakeet recovery isn't supported yet; the audio is kept."
        );
    }
}
