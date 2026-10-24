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
    fail_start: bool,
    emit_during_stop: bool,
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
        if self.fail_start {
            return Err(TapError::platform("fixture failed start"));
        }
        Ok(self.format())
    }
    fn stop(&mut self) -> Result<(), TapError> {
        if self.emit_during_stop {
            let mut callbacks = self.callbacks.lock().unwrap();
            let sink = callbacks.last_mut().unwrap();
            emit(&mut **sink);
            sink.on_error(TapError::platform("fixture callback during native stop"));
        }
        Ok(())
    }
}
fn setup() -> (CaptureSupervisor, Arc<ManualClock>, Callbacks, SinkHandle) {
    setup_behavior(None, false)
}
fn setup_behavior(
    fail_at: Option<usize>,
    emit_during_stop: bool,
) -> (CaptureSupervisor, Arc<ManualClock>, Callbacks, SinkHandle) {
    let clock = ManualClock::new();
    let callbacks: Callbacks = Arc::default();
    let factory = callbacks.clone();
    let sink = SinkHandle::new();
    let downstream = sink.clone();
    let mut generation = 0;
    let supervisor = CaptureSupervisor::new(
        SupervisorConfig::default(),
        clock.clone(),
        move || {
            let fail_start = fail_at == Some(generation);
            generation += 1;
            Ok(Box::new(CallbackTap {
                fail_start,
                emit_during_stop,
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

#[test]
fn a_retired_healthy_callback_cannot_deliver_into_the_replacement() {
    let (mut supervisor, clock, callbacks, downstream) = setup();
    supervisor.start().unwrap();
    clock.advance(Duration::from_secs(6));
    assert_eq!(
        supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
        PollOutcome::Rebuilt
    );
    emit(&mut *callbacks.lock().unwrap()[0]);
    callbacks.lock().unwrap()[0].on_error(TapError::platform("retired fixture error"));
    assert!(
        downstream.samples().is_empty(),
        "retired frames must not update downstream health counters or PCM"
    );
    assert!(downstream.errors().is_empty());
    emit(&mut *callbacks.lock().unwrap()[1]);
    assert_eq!(downstream.samples(), vec![0.25]);
}

#[test]
fn stop_detach_and_drop_revoke_callback_delivery() {
    for operation in ["stop", "detach", "drop"] {
        let (mut supervisor, _, callbacks, downstream) = setup();
        supervisor.start().unwrap();
        emit(&mut *callbacks.lock().unwrap()[0]);
        let mut detached = None;
        match operation {
            "stop" => supervisor.stop().unwrap(),
            "detach" => detached = supervisor.take_running_tap(),
            "drop" => drop(supervisor),
            _ => unreachable!(),
        }
        emit(&mut *callbacks.lock().unwrap()[0]);
        callbacks.lock().unwrap()[0].on_error(TapError::platform("late fixture error"));
        assert_eq!(
            downstream.samples(),
            vec![0.25],
            "{operation} must revoke frames"
        );
        assert!(
            downstream.errors().is_empty(),
            "{operation} must revoke errors"
        );
        drop(detached);
    }
}

#[test]
fn failed_initial_and_recovery_starts_revoke_their_retained_sinks() {
    for fail_at in [0, 1] {
        let (mut supervisor, clock, callbacks, downstream) = setup_behavior(Some(fail_at), false);
        if fail_at == 0 {
            assert!(supervisor.start().is_err());
        } else {
            supervisor.start().unwrap();
            clock.advance(Duration::from_secs(6));
            assert_eq!(
                supervisor.poll(TapActivity::default(), &OutputActivity::Unknown),
                PollOutcome::Retrying
            );
        }
        emit(&mut *callbacks.lock().unwrap()[fail_at]);
        callbacks.lock().unwrap()[fail_at].on_error(TapError::platform("failed attempt callback"));
        assert!(downstream.samples().is_empty());
        assert!(downstream.errors().is_empty());
    }
}

#[test]
fn admission_is_revoked_before_native_stop_and_a_fresh_session_can_deliver() {
    let (mut supervisor, _, callbacks, downstream) = setup_behavior(None, true);
    supervisor.start().unwrap();
    supervisor.stop().unwrap();
    assert!(
        downstream.samples().is_empty(),
        "native stop can trigger callbacks and must run after revocation"
    );
    assert!(downstream.errors().is_empty());
    supervisor.start().unwrap();
    emit(&mut *callbacks.lock().unwrap()[0]);
    emit(&mut *callbacks.lock().unwrap()[1]);
    assert_eq!(
        downstream.samples(),
        vec![0.25],
        "only the fresh attempt delivers"
    );
}
