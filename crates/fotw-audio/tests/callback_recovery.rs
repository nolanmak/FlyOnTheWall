//! Reported device faults must not wait for watchdog starvation.
use fotw_audio::supervisor::{CaptureSupervisor, PollOutcome, SupervisorConfig};
use fotw_audio::testing::{ManualClock, SinkHandle};
use fotw_audio::watchdog::{OutputActivity, TapActivity};
use fotw_audio::{AudioTap, FrameSink, SampleFormat, StreamFormat, TapError, TapId};
use fotw_audio::{CaptureTimestamp, FrameFlags};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Callbacks = Arc<Mutex<Vec<Box<dyn FrameSink>>>>;
struct CallbackTap {
    id: TapId,
    callbacks: Callbacks,
}
impl AudioTap for CallbackTap {
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
        self.callbacks.lock().unwrap().push(sink);
        Ok(self.format())
    }
    fn stop(&mut self) -> Result<(), TapError> {
        Ok(())
    }
}
fn setup() -> (CaptureSupervisor, Arc<ManualClock>, Callbacks, SinkHandle) {
    let clock = ManualClock::new();
    let callbacks: Callbacks = Arc::default();
    let factory = callbacks.clone();
    let sink = SinkHandle::new();
    let downstream = sink.clone();
    let supervisor = CaptureSupervisor::new(
        SupervisorConfig::default(),
        clock.clone(),
        move || {
            Ok(Box::new(CallbackTap {
                id: TapId::system_default(),
                callbacks: factory.clone(),
            }))
        },
        move || downstream.sink(),
    );
    (supervisor, clock, callbacks, sink)
}
fn emit(sink: &mut dyn FrameSink) {
    sink.on_frames(
        &[0.25],
        CaptureTimestamp {
            host_ns: 0,
            device_frames: 0,
        },
        FrameFlags::empty(),
    );
}
#[test]
fn callback_fault_rebuilds_on_next_poll_and_rejects_further_pcm() {
    let (mut supervisor, clock, callbacks, downstream) = setup();
    supervisor.start().unwrap();
    clock.advance(Duration::from_millis(100));
    {
        let mut sinks = callbacks.lock().unwrap();
        emit(&mut *sinks[0]);
        sinks[0].on_error(TapError::StreamFormatChanged);
        emit(&mut *sinks[0]);
    }
    assert_eq!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Rebuilt
    );
    assert_eq!(
        downstream.samples(),
        vec![0.25],
        "faulted PCM must not reach the WAL sink"
    );
    callbacks.lock().unwrap()[0].on_error(TapError::StreamFormatChanged);
    assert_ne!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Rebuilt,
        "a retired callback must not fault the replacement"
    );
    emit(&mut *callbacks.lock().unwrap()[1]);
    assert_eq!(downstream.samples(), vec![0.25, 0.25]);
}
#[test]
fn callback_faults_obey_spacing_budget_and_stop() {
    let (mut supervisor, clock, callbacks, _) = setup();
    supervisor.start().unwrap();
    for generation in 0..3 {
        callbacks.lock().unwrap()[generation]
            .on_error(TapError::DeviceInvalidated(TapId::system_default()));
        if generation > 0 {
            assert_eq!(
                supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
                PollOutcome::Waiting
            );
            clock.advance(Duration::from_secs(15));
        }
        assert_eq!(
            supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
            PollOutcome::Rebuilt
        );
    }
    callbacks.lock().unwrap()[3].on_error(TapError::StreamFormatChanged);
    assert_eq!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Abandoned
    );
    assert_eq!(callbacks.lock().unwrap().len(), 4);
    supervisor.stop().unwrap();
    assert_eq!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Stopped
    );
}

#[test]
fn first_fault_timestamp_survives_repeated_errors_exhaustion_and_manual_retry() {
    use fotw_audio::supervisor::{GapReason, HealthEvent};
    let (mut supervisor, clock, callbacks, _) = setup();
    supervisor.start().unwrap();
    for generation in 0..3 {
        callbacks.lock().unwrap()[generation].on_error(TapError::StreamFormatChanged);
        clock.advance(Duration::from_secs(15));
        assert_eq!(
            supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
            PollOutcome::Rebuilt
        );
    }
    supervisor.drain_events();
    let first_fault = clock.now_ns();
    callbacks.lock().unwrap()[3].on_error(TapError::StreamFormatChanged);
    assert_eq!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Abandoned
    );
    clock.advance(Duration::from_secs(20));
    callbacks.lock().unwrap()[3].on_error(TapError::StreamFormatChanged);
    assert!(supervisor.request_retry());
    assert_eq!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Rebuilt
    );
    let events = supervisor.drain_events();
    let gap = events
        .iter()
        .find_map(|event| match event {
            HealthEvent::Recovered { gaps, .. } => gaps.first(),
            _ => None,
        })
        .expect("recovery reports the entire outage");
    assert_eq!(gap.start_ns, first_fault);
    assert_eq!(gap.end_ns, clock.now_ns());
    assert_eq!(gap.reason, GapReason::CallbackFault);
}
