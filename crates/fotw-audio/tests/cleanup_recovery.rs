//! Native cleanup evidence must survive recovery, Stop, and new sessions.
use fotw_audio::supervisor::{CaptureSupervisor, PollOutcome, SupervisorConfig};
use fotw_audio::testing::{ManualClock, SinkHandle};
use fotw_audio::watchdog::{OutputActivity, TapActivity};
use fotw_audio::{AudioTap, CleanupState, FrameSink, SampleFormat, StreamFormat, TapError, TapId};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Tap {
    id: TapId,
    sink: Arc<Mutex<Option<Box<dyn FrameSink>>>>,
    cleanup: CleanupState,
    fail_start: bool,
}
impl AudioTap for Tap {
    fn id(&self) -> &TapId {
        &self.id
    }
    fn format(&self) -> StreamFormat {
        StreamFormat::new(48000, 1, SampleFormat::F32)
    }
    fn format_is_authoritative(&self) -> bool {
        true
    }
    fn start(&mut self, sink: Box<dyn FrameSink>) -> Result<StreamFormat, TapError> {
        *self.sink.lock().unwrap() = Some(sink);
        if self.fail_start {
            Err(self.failure())
        } else {
            Ok(self.format())
        }
    }
    fn stop(&mut self) -> Result<(), TapError> {
        Err(self.failure())
    }
}
impl Tap {
    fn failure(&self) -> TapError {
        TapError::Cleanup {
            state: self.cleanup,
            message: "injected native failure".into(),
        }
    }
}
type Callback = Arc<Mutex<Option<Box<dyn FrameSink>>>>;
type Setup = (
    CaptureSupervisor,
    Arc<ManualClock>,
    Arc<Mutex<usize>>,
    Callback,
);
fn setup(cleanup: CleanupState, fail_start: bool) -> Setup {
    let clock = ManualClock::new();
    let opens = Arc::new(Mutex::new(0));
    let opened = opens.clone();
    let sink = Arc::new(Mutex::new(None));
    let callback = sink.clone();
    let downstream = SinkHandle::new();
    let supervisor = CaptureSupervisor::new(
        SupervisorConfig::default(),
        clock.clone(),
        move || {
            *opened.lock().unwrap() += 1;
            Ok(Box::new(Tap {
                id: TapId::system_default(),
                sink: callback.clone(),
                cleanup,
                fail_start,
            }))
        },
        move || downstream.sink(),
    );
    (supervisor, clock, opens, sink)
}
#[test]
fn retained_native_resources_block_automatic_manual_and_new_session_reopening() {
    for state in [CleanupState::Stopped, CleanupState::Unconfirmed] {
        let (mut supervisor, clock, opens, sink) = setup(state, false);
        supervisor.start().unwrap();
        sink.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .on_error(TapError::StreamFormatChanged);
        assert_eq!(
            supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
            PollOutcome::Abandoned
        );
        assert!(!supervisor.request_retry());
        clock.advance(Duration::from_secs(60));
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown);
        assert_eq!(*opens.lock().unwrap(), 1);
        assert_eq!(supervisor.stop().unwrap_err().cleanup_state(), Some(state));
        assert!(supervisor.start().is_err());
        assert_eq!(*opens.lock().unwrap(), 1);
    }
}
#[test]
fn failed_start_cleanup_blocks_optional_source_retry() {
    let (mut supervisor, clock, opens, _) = setup(CleanupState::Unconfirmed, true);
    assert!(supervisor.start_recovering().is_err());
    clock.advance(Duration::from_secs(60));
    assert_eq!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Abandoned
    );
    assert!(!supervisor.request_retry());
    assert_eq!(*opens.lock().unwrap(), 1);
}
#[test]
fn confirmed_release_allows_recovery_even_when_stop_reported_an_error() {
    let (mut supervisor, _, opens, sink) = setup(CleanupState::Released, false);
    supervisor.start().unwrap();
    sink.lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .on_error(TapError::StreamFormatChanged);
    assert_eq!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Rebuilt
    );
    assert_eq!(*opens.lock().unwrap(), 2);
}
