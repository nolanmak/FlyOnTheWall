//! Encrypted detection preferences and bounded, off-main-thread decline writes.
use crate::audit::{AuditKind, AuditLog};
use fotw_store::Db;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
};

const SETTINGS_KEY: &str = "detection.suppressed_apps.v1";
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    apps: BTreeSet<String>,
}
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 255
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn read(db: &Db) -> Result<Saved, String> {
    let Some(raw) = db
        .get_setting(SETTINGS_KEY)
        .map_err(|_| "Could not read detection preferences.")?
    else {
        return Ok(Saved::default());
    };
    let saved: Saved = if raw.len() <= 65_536 {
        serde_json::from_str(&raw).ok()
    } else {
        None
    }
    .ok_or("Saved detection preferences are invalid.")?;
    if saved.apps.len() > 128 || saved.apps.iter().any(|key| !valid_key(key)) {
        return Err("Saved detection preferences are invalid.".into());
    }
    Ok(saved)
}
/// The two explicit dismissal choices. Manual consent cancellation is separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclineAnswer {
    /// Snooze automatic offers for the current session.
    NotNow,
    /// Suppress this application's automatic offers across restarts.
    NeverForThisApp,
}
impl DeclineAnswer {
    fn label(self) -> &'static str {
        match self {
            Self::NotNow => "not_now",
            Self::NeverForThisApp => "never_for_this_app",
        }
    }
}
struct Request {
    app: String,
    answer: DeclineAnswer,
}
struct Completion {
    pending: AtomicBool,
    result: Mutex<Option<Result<(), String>>>,
}
/// One outstanding write at most. Native event handlers perform no disk IO.
pub struct DetectionPreferences {
    requests: SyncSender<Request>,
    completion: Arc<Completion>,
    initial_apps: Vec<String>,
}
impl DetectionPreferences {
    /// Read encrypted preferences before native launch, then move the connection
    /// to a dedicated writer. Does not access credentials, capture or providers.
    ///
    /// # Errors
    /// Invalid saved preferences, database failure or unavailable worker thread.
    pub fn spawn(mut db: Db, audit: AuditLog) -> Result<Self, String> {
        let initial_apps = read(&db)?.apps.into_iter().collect();
        Self::worker(initial_apps, move |request| save(&mut db, &audit, request))
    }
    fn worker(
        initial_apps: Vec<String>,
        mut write: impl FnMut(Request) -> Result<(), String> + Send + 'static,
    ) -> Result<Self, String> {
        let (requests, receiver) = mpsc::sync_channel(1);
        let completion = Arc::new(Completion {
            pending: AtomicBool::new(false),
            result: Mutex::new(None),
        });
        let reply = completion.clone();
        std::thread::Builder::new()
            .name("fotw-detection-preferences".into())
            .spawn(move || {
                // Dropping the sender still drains the one accepted request.
                for request in receiver {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(request)))
                            .unwrap_or_else(|_| {
                                Err(
                            "Detection preference writer failed; the change may not be saved."
                                .into(),
                        )
                            });
                    *reply
                        .result
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
                }
            })
            .map_err(|_| "Could not start the detection preference writer.")?;
        Ok(Self {
            requests,
            completion,
            initial_apps,
        })
    }
    pub(crate) fn initial_apps(&self) -> &[String] {
        &self.initial_apps
    }
    pub(crate) fn submit(&self, app: String, answer: DeclineAnswer) -> Result<(), String> {
        if !valid_key(&app) {
            return Err("The detected application is invalid.".into());
        }
        if self
            .completion
            .pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("A detection preference is still being saved; try again.".into());
        }
        if self.requests.try_send(Request { app, answer }).is_err() {
            self.completion.pending.store(false, Ordering::Release);
            return Err("Detection preferences could not be queued; try again.".into());
        }
        Ok(())
    }
    pub(crate) fn poll(&self) -> Option<Result<(), String>> {
        let result = self.completion.result.try_lock().ok()?.take()?;
        self.completion.pending.store(false, Ordering::Release);
        Some(result)
    }
    pub(crate) fn pending(&self) -> bool {
        self.completion.pending.load(Ordering::Acquire)
    }
}
fn save_app(db: &mut Db, app: &str) -> Result<(), String> {
    let mut saved = read(db).map_err(|_| "Saved detection preferences could not be read; this choice only applies until restart.")?;
    saved.apps.insert(app.into());
    if saved.apps.len() > 128 {
        return Err(
            "Too many saved detection exclusions; this choice only applies until restart.".into(),
        );
    }
    let encoded =
        serde_json::to_string(&saved).map_err(|_| "Could not encode detection preferences.")?;
    db.put_setting(SETTINGS_KEY, &encoded).map_err(|_| {
        "Detection preference was not saved; this choice only applies until restart.".into()
    })
}
fn save(db: &mut Db, audit: &AuditLog, request: Request) -> Result<(), String> {
    let persisted = if request.answer == DeclineAnswer::NeverForThisApp {
        save_app(db, &request.app)
    } else {
        Ok(())
    };
    // A preference failure must not erase the independent fact that the user
    // declined. Always attempt the content-free audit, including partial failure.
    let audited = audit.record(AuditKind::DetectionDeclined {
        app: request.app,
        answer: request.answer.label().into(),
    });
    match (persisted, audited) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Err(_), Err(_)) => Err("Detection preference and decline audit were not saved; suppression only applies until restart.".into()),
        (Ok(()), Err(_)) => Err(if request.answer == DeclineAnswer::NeverForThisApp {
            "Detection preference saved, but the decline audit could not be written."
        } else { "Detection snoozed, but the decline audit could not be written." }.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn db() -> Db {
        Db::open_in_memory(&fotw_store::DbKey::from_bytes([0x29; 32])).unwrap()
    }
    fn finish(p: &DetectionPreferences) -> Result<(), String> {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(result) = p.poll() {
                return result;
            }
            assert!(Instant::now() < until, "preference writer did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn never_survives_an_encrypted_library_restart_and_keeps_other_settings() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("library.db");
        let key = fotw_store::DbKey::from_bytes([0x19; 32]);
        let audit = AuditLog::at(&root.path().join("sessions"));
        let mut db = Db::open(&path, &key).unwrap();
        db.put_setting("unrelated", "preserved").unwrap();
        let p = DetectionPreferences::spawn(db, audit.clone()).unwrap();
        p.submit("us.zoom.xos".into(), DeclineAnswer::NeverForThisApp)
            .unwrap();
        finish(&p).unwrap();
        // Reopen a real encrypted connection: not a reused in-memory set.
        let reopened = Db::open(&path, &key).unwrap();
        assert_eq!(
            reopened.get_setting("unrelated").unwrap().as_deref(),
            Some("preserved")
        );
        let next = DetectionPreferences::spawn(reopened, audit.clone()).unwrap();
        assert_eq!(next.initial_apps(), &["us.zoom.xos"]);
        assert_eq!(audit.read().unwrap().len(), 1);
        assert!(matches!(&audit.read().unwrap()[0].kind,
            AuditKind::DetectionDeclined {app, answer} if app == "us.zoom.xos" && answer == "never_for_this_app"));
    }
    #[test]
    fn not_now_audits_without_persisting_a_permanent_suppression() {
        let root = tempfile::tempdir().unwrap();
        let mut db = db();
        let audit = AuditLog::at(&root.path().join("sessions"));
        save(
            &mut db,
            &audit,
            Request {
                app: "us.zoom.xos".into(),
                answer: DeclineAnswer::NotNow,
            },
        )
        .unwrap();
        assert!(read(&db).unwrap().apps.is_empty());
        assert!(db.get_setting(SETTINGS_KEY).unwrap().is_none());
        assert!(matches!(&audit.read().unwrap()[0].kind,
            AuditKind::DetectionDeclined {answer, ..} if answer == "not_now"));
    }
    #[test]
    fn corrupt_or_oversized_preferences_are_preserved_and_cannot_enable_prompts() {
        let mut db = db();
        for bad in [
            "broken".to_owned(),
            "{}".into(),
            r#"{"apps":["bad/path"]}"#.into(),
            r#"{"apps":[],"unknown":true}"#.into(),
            " ".repeat(65_537),
        ] {
            db.put_setting(SETTINGS_KEY, &bad).unwrap();
            assert!(read(&db).is_err());
            assert_eq!(
                db.get_setting(SETTINGS_KEY).unwrap().as_deref(),
                Some(bad.as_str())
            );
        }
    }
    #[test]
    fn blocked_writer_is_bounded_and_keeps_quit_ownership_until_completion_is_observed() {
        let (entered, started) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let p = DetectionPreferences::worker(vec![], move |_| {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
        p.submit("us.zoom.xos".into(), DeclineAnswer::NotNow)
            .unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(p.pending());
        assert!(p.poll().is_none());
        assert!(
            p.submit(
                "com.microsoft.teams2".into(),
                DeclineAnswer::NeverForThisApp
            )
            .is_err()
        );
        release.send(()).unwrap();
        finish(&p).unwrap();
        assert!(!p.pending());
    }
    #[test]
    fn audit_failure_is_reported_without_erasing_the_saved_preference() {
        let root = tempfile::tempdir().unwrap();
        let audit = AuditLog::at(&root.path().join("sessions"));
        std::fs::create_dir(audit.path()).unwrap();
        let mut db = db();
        let error = save(
            &mut db,
            &audit,
            Request {
                app: "us.zoom.xos".into(),
                answer: DeclineAnswer::NeverForThisApp,
            },
        )
        .unwrap_err();
        assert!(error.contains("saved, but"));
        assert!(read(&db).unwrap().apps.contains("us.zoom.xos"));
    }
    #[test]
    fn writer_panic_returns_a_failure_instead_of_leaving_permanent_pending_state() {
        let p = DetectionPreferences::worker(vec![], |_| panic!("synthetic failure")).unwrap();
        assert!(p.submit("bad/path".into(), DeclineAnswer::NotNow).is_err());
        assert!(!p.pending());
        p.submit("us.zoom.xos".into(), DeclineAnswer::NotNow)
            .unwrap();
        assert!(finish(&p).is_err());
        assert!(!p.pending());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_preference_write_does_not_block_controls_but_quit_waits_for_it() {
        use fotw_shell::{MenuAction, Monotonic, PromptChoice, SurfaceEvent, SurfaceHost};
        use std::sync::mpsc;
        let root = tempfile::tempdir().unwrap();
        let gate = crate::native_bridge::RenderGate::new(Duration::from_secs(2));
        let rec = Arc::new(
            crate::recording::DaemonRecorder::with_parts(
                root.path().join("sessions"),
                tokio::runtime::Handle::current(),
                crate::session::SegmentTap::default(),
                crate::recording::ReadyTap::default(),
                Box::new(|| panic!("this fixture cannot capture")),
                Box::new(|| panic!("this fixture cannot configure speech")),
                Box::new(|_, _| None),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .with_start_visibility(gate.admission()),
        );
        let bridge = crate::native_bridge::RecorderBridge::spawn(rec, gate).unwrap();
        let (actions, _requests) = mpsc::sync_channel(4);
        let (entered, started) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let prefs = DetectionPreferences::worker(vec![], move |_| {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        })
        .unwrap();
        prefs
            .submit("us.zoom.xos".into(), DeclineAnswer::NeverForThisApp)
            .unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut host = crate::native_host::NativeHost::new(
            bridge,
            fotw_shell::HotkeyMap::defaults(),
            "Synthetic notice".into(),
            actions,
        )
        .with_detection_preferences(prefs);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !host.view().menu.record.enabled {
            assert!(Instant::now() < deadline);
            host.tick(Monotonic::ZERO);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        host.on_event(
            SurfaceEvent::Menu(MenuAction::ToggleRecording),
            Monotonic::ZERO,
        );
        assert!(
            host.view().prompt.is_some(),
            "blocked storage froze manual controls"
        );
        host.on_event(SurfaceEvent::Prompt(PromptChoice::NotNow), Monotonic::ZERO);
        assert!(host.view().prompt.is_none());
        host.on_event(SurfaceEvent::Menu(MenuAction::Quit), Monotonic::ZERO);
        host.tick(Monotonic::ZERO);
        assert!(!host.should_exit(), "quit discarded an accepted preference");
        assert!(
            host.view()
                .tray
                .tooltip
                .contains("Saving detection preferences")
        );
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !host.should_exit() {
            assert!(Instant::now() < deadline);
            host.tick(Monotonic::ZERO);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    #[test]
    fn failed_preference_write_still_attempts_the_decline_audit_and_preserves_the_old_set() {
        let root = tempfile::tempdir().unwrap();
        let audit = AuditLog::at(&root.path().join("sessions"));
        let mut db = db();
        let previous = serde_json::to_string(&Saved {
            apps: (0..128).map(|n| format!("example.app{n}")).collect(),
        })
        .unwrap();
        db.put_setting(SETTINGS_KEY, &previous).unwrap();
        assert!(
            save(
                &mut db,
                &audit,
                Request {
                    app: "us.zoom.xos".into(),
                    answer: DeclineAnswer::NeverForThisApp,
                }
            )
            .is_err()
        );
        assert_eq!(
            db.get_setting(SETTINGS_KEY).unwrap().as_deref(),
            Some(previous.as_str())
        );
        let events = audit.read().unwrap();
        assert_eq!(
            events.len(),
            1,
            "failed persistence erased the explicit decline audit"
        );
        assert!(
            matches!(&events[0].kind, AuditKind::DetectionDeclined { app, answer }
            if app=="us.zoom.xos" && answer=="never_for_this_app")
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unrelated_success_cannot_acknowledge_an_earlier_failed_native_write() {
        use fotw_shell::{MenuAction, Monotonic, SurfaceEvent, SurfaceHost};
        let root = tempfile::tempdir().unwrap();
        let gate = crate::native_bridge::RenderGate::new(Duration::from_secs(2));
        let rec = Arc::new(
            crate::recording::DaemonRecorder::with_parts(
                root.path().join("sessions"),
                tokio::runtime::Handle::current(),
                crate::session::SegmentTap::default(),
                crate::recording::ReadyTap::default(),
                Box::new(|| panic!("this fixture cannot capture")),
                Box::new(|| panic!("this fixture cannot configure speech")),
                Box::new(|_, _| None),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .with_start_visibility(gate.admission()),
        );
        let bridge = crate::native_bridge::RecorderBridge::spawn(rec, gate).unwrap();
        let (actions, _requests) = mpsc::sync_channel(4);
        let prefs = DetectionPreferences::worker(vec![], |_| Ok(())).unwrap();
        let completion = prefs.completion.clone();
        let mut host = crate::native_host::NativeHost::new(
            bridge,
            fotw_shell::HotkeyMap::defaults(),
            "Synthetic notice".into(),
            actions,
        )
        .with_detection_preferences(prefs);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !host.view().menu.record.enabled {
            assert!(Instant::now() < deadline);
            host.tick(Monotonic::ZERO);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Inject consecutive worker completions, not private host flags.
        *completion.result.lock().unwrap() = Some(Err("Synthetic preference failure".into()));
        host.tick(Monotonic::ZERO);
        assert!(
            host.view()
                .tray
                .tooltip
                .contains("Synthetic preference failure")
        );
        *completion.result.lock().unwrap() = Some(Ok(()));
        host.tick(Monotonic::ZERO);
        host.on_event(SurfaceEvent::Menu(MenuAction::Quit), Monotonic::ZERO);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            host.tick(Monotonic::ZERO);
            if host.should_exit() || host.view().menu.quit.label.contains("unsaved preferences") {
                break;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            !host.should_exit(),
            "unrelated success silently acknowledged an earlier failure"
        );
        host.on_event(SurfaceEvent::Menu(MenuAction::Quit), Monotonic::ZERO);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !host.should_exit() {
            assert!(Instant::now() < deadline);
            host.tick(Monotonic::ZERO);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}
