//! Read-only background activity samples for native consent prompts.
use crate::detect::{Detection, Detector, DetectorConfig, NoCalendar};
use fotw_audio::activity::ActivitySnapshot;
use fotw_shell::Monotonic;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Duration;

const MAX_SAMPLE_AGE_NS: u64 = 3_000_000_000;
/// One read-only observation, timestamped before the potentially slow OS query.
#[derive(Debug)]
pub struct ActivitySample {
    /// Continuous host nanoseconds at query start, including macOS sleep.
    pub captured_at_ns: u64,
    /// Process activity metadata only; never audio or credentials.
    pub snapshot: Result<ActivitySnapshot, String>,
}
/// Bounded sample inbox and the existing pure detector. No native calls here.
pub struct NativeDetection {
    detector: Detector,
    receiver: Receiver<ActivitySample>,
    latest: Option<ActivitySample>,
}
impl NativeDetection {
    /// Create an injectable, one-sample mailbox. A busy UI cannot accumulate history.
    #[must_use]
    pub fn channel(config: DetectorConfig) -> (Self, SyncSender<ActivitySample>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        (
            Self {
                detector: Detector::new(config),
                receiver,
                latest: None,
            },
            sender,
        )
    }
    pub(crate) fn poll(&mut self, now: Monotonic) -> Detection {
        self.poll_at(now, fotw_audio::clock::host_ns())
    }
    fn poll_at(&mut self, now: Monotonic, wall: u64) -> Detection {
        // Reset dwell across an observation gap even if a new sample is already
        // queued. Fresh evidence after a UI stall cannot prove the missing time.
        let interrupted = self
            .latest
            .as_ref()
            .is_some_and(|s| wall.saturating_sub(s.captured_at_ns) > MAX_SAMPLE_AGE_NS);
        let cleared = interrupted.then(|| {
            self.detector
                .poll(now, Err("activity sample expired"), &NoCalendar)
        });
        // One mailbox slot plus a closure check: a final queued sample must
        // not conceal that its publisher has already stopped. Keep UI work bounded.
        for _ in 0..2 {
            match self.receiver.try_recv() {
                Ok(sample) => self.latest = Some(sample),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.latest = None;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        let observation = self.latest.as_ref().filter(|sample| {
            sample.captured_at_ns <= wall
                && wall.saturating_sub(sample.captured_at_ns) <= MAX_SAMPLE_AGE_NS
        });
        let snapshot = observation.map_or(Err("activity unavailable"), |s| {
            s.snapshot.as_ref().map_err(String::as_str)
        });
        let update = self.detector.poll(now, snapshot, &NoCalendar);
        if matches!(cleared, Some(Detection::Clear))
            && matches!(update, Detection::Idle | Detection::Hold)
        {
            Detection::Clear
        } else {
            update
        }
    }
    pub(crate) fn restore_suppressions(&mut self, apps: &[String]) {
        self.detector.restore_suppressions(apps.iter().cloned());
    }
    pub(crate) fn suppress(&mut self, app: &str) {
        self.detector.suppress_app(app);
    }
    pub(crate) fn snooze(&mut self, now: Monotonic) {
        self.detector.snooze(now);
    }
    pub(crate) fn started(&mut self) {
        self.detector.record_started();
    }
}
pub(crate) fn spawn(
    probe: std::sync::Arc<dyn fotw_audio::activity::ActivityProbe>,
    stop: crate::session::StopSignal,
    config: DetectorConfig,
) -> NativeDetection {
    let (input, sender) = NativeDetection::channel(config);
    tokio::spawn(async move {
        while !stop.is_stopped() {
            let captured_at_ns = fotw_audio::clock::host_ns();
            let query = probe.clone();
            // Exactly one outstanding query. Native Core Audio reads never run
            // on AppKit, and slow queries cannot produce fresh-looking samples.
            let task =
                tokio::task::spawn_blocking(move || query.snapshot().map_err(|e| e.to_string()));
            let snapshot = tokio::select! {
                () = stop.wait() => break,
                result = task => result.unwrap_or_else(|_| Err("activity query failed".into())),
            };
            let sample = ActivitySample {
                captured_at_ns,
                snapshot,
            };
            if matches!(
                sender.try_send(sample),
                Err(std::sync::mpsc::TrySendError::Disconnected(_))
            ) {
                break;
            }
            tokio::select! {
                () = stop.wait() => break,
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    });
    input
}
#[cfg(test)]
mod tests {
    use super::*;
    use fotw_audio::activity::{AudioClient, InputDevice, Transport};
    fn call() -> ActivitySnapshot {
        ActivitySnapshot {
            clients: vec![
                AudioClient::new(123, Some("us.zoom.xos"))
                    .with_input(true)
                    .with_output(true),
            ],
            default_input: Some(InputDevice::new(
                "Synthetic input",
                Transport::BuiltIn,
                true,
            )),
        }
    }
    fn setup() -> (NativeDetection, SyncSender<ActivitySample>) {
        NativeDetection::channel(DetectorConfig {
            dwell: Duration::from_secs(2),
            ..DetectorConfig::default()
        })
    }
    fn offer(sender: &SyncSender<ActivitySample>, at: u64) {
        sender
            .try_send(ActivitySample {
                captured_at_ns: at,
                snapshot: Ok(call()),
            })
            .unwrap();
    }
    #[test]
    fn fresh_conjunction_arms_then_stale_or_failed_sampling_withdraws() {
        let (mut input, sender) = setup();
        let wall = 0;
        offer(&sender, wall);
        assert_eq!(input.poll_at(Monotonic::ZERO, wall), Detection::Idle);
        offer(&sender, wall + 2_000_000_000);
        assert!(matches!(
            input.poll_at(Monotonic::from_secs(2), wall + 2_000_000_000),
            Detection::Arm(_)
        ));
        assert_eq!(
            input.poll_at(Monotonic::from_secs(6), wall + 6_000_000_000),
            Detection::Clear
        );
        offer(&sender, wall + 7_000_000_000);
        assert_eq!(
            input.poll_at(Monotonic::from_secs(7), wall + 7_000_000_000),
            Detection::Idle
        );
        sender
            .try_send(ActivitySample {
                captured_at_ns: wall + 9_000_000_000,
                snapshot: Err("synthetic probe error".into()),
            })
            .unwrap();
        assert_eq!(
            input.poll_at(Monotonic::from_secs(9), wall + 9_000_000_000),
            Detection::Idle
        );
    }
    #[test]
    fn fresh_sample_after_a_sampling_gap_cannot_credit_unobserved_dwell() {
        let (mut input, sender) = setup();
        let wall = 0;
        offer(&sender, wall);
        input.poll_at(Monotonic::ZERO, wall);
        offer(&sender, wall + 20_000_000_000);
        assert_eq!(
            input.poll_at(Monotonic::from_secs(20), wall + 20_000_000_000),
            Detection::Idle
        );
        offer(&sender, wall + 22_000_000_000);
        assert!(matches!(
            input.poll_at(Monotonic::from_secs(22), wall + 22_000_000_000),
            Detection::Arm(_)
        ));
        drop(sender);
        assert_eq!(
            input.poll_at(Monotonic::from_secs(22), wall + 22_000_000_000),
            Detection::Clear
        );
    }
    #[test]
    fn closed_publisher_with_a_final_queued_sample_withdraws_the_offer() {
        let (mut input, sender) = NativeDetection::channel(DetectorConfig {
            dwell: Duration::ZERO,
            ..Default::default()
        });
        offer(&sender, 0);
        assert!(matches!(
            input.poll_at(Monotonic::ZERO, 0),
            Detection::Arm(_)
        ));
        offer(&sender, 1);
        drop(sender);
        assert_eq!(input.poll_at(Monotonic::ZERO, 1), Detection::Clear);
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn blocked_probe_cannot_block_polling_or_shutdown_or_spawn_more_queries() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };
        struct Probe {
            entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            exited: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: Mutex<mpsc::Receiver<()>>,
            calls: AtomicUsize,
        }
        impl fotw_audio::activity::ActivityProbe for Probe {
            fn snapshot(&self) -> Result<ActivitySnapshot, fotw_audio::TapError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                let _ = self.release.lock().unwrap().recv();
                let _ = self.exited.lock().unwrap().take().unwrap().send(());
                Ok(call())
            }
        }
        struct Release(Option<mpsc::Sender<()>>);
        impl Drop for Release {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let (enter, entered) = tokio::sync::oneshot::channel();
        let (exit, exited) = tokio::sync::oneshot::channel();
        let (release, receiver) = mpsc::channel();
        let release = Release(Some(release));
        let probe = Arc::new(Probe {
            entered: Mutex::new(Some(enter)),
            exited: Mutex::new(Some(exit)),
            release: Mutex::new(receiver),
            calls: AtomicUsize::new(0),
        });
        let stop = crate::session::StopSignal::new();
        let mut input = spawn(probe.clone(), stop.clone(), DetectorConfig::default());
        tokio::time::timeout(Duration::from_secs(2), entered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(input.poll(Monotonic::ZERO), Detection::Idle);
        stop.stop();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    input.receiver.try_recv(),
                    Err(mpsc::TryRecvError::Disconnected)
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("sampler shutdown waited for the blocked OS query");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), exited)
            .await
            .unwrap()
            .unwrap();
    }
}
