//! Session-owned health and retry mailbox. Never touched by audio callbacks.

use std::sync::{Arc, Mutex};

use fotw_audio::supervisor::{DeliveryState, RecoveryStatus};
use fotw_web::recorder::{CaptureDelivery, CaptureHealth, CaptureSource, SourceCaptureStatus};

#[derive(Debug, Default)]
struct Inner {
    health: Option<CaptureHealth>,
    retries: u8,
    closed: bool,
}

/// One recording's monitor. A new session always gets a separate instance;
/// closing it fences late pump updates as well as queued retry requests.
#[derive(Debug, Clone, Default)]
pub struct CaptureMonitor(Arc<Mutex<Inner>>);

fn bit(source: CaptureSource) -> u8 {
    match source {
        CaptureSource::System => 1,
        CaptureSource::Mic => 2,
    }
}

fn source_mut(health: &mut CaptureHealth, source: CaptureSource) -> &mut SourceCaptureStatus {
    match source {
        CaptureSource::System => &mut health.system,
        CaptureSource::Mic => &mut health.mic,
    }
}

impl CaptureMonitor {
    /// Read bounded status, without native error messages or recorded content.
    pub fn snapshot(&self) -> Option<CaptureHealth> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).health
    }

    /// Publish only from the control/pump thread, after polling a source.
    pub fn publish(&self, source: CaptureSource, status: RecoveryStatus) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return;
        }
        let unavailable = SourceCaptureStatus {
            state: CaptureDelivery::Unavailable,
            attempts: 0,
            max_attempts: 0,
            retry_in_ms: None,
        };
        let health = inner.health.get_or_insert(CaptureHealth {
            system: unavailable,
            mic: unavailable,
        });
        *source_mut(health, source) = SourceCaptureStatus {
            state: match status.state {
                DeliveryState::Confirming => CaptureDelivery::Confirming,
                DeliveryState::Healthy => CaptureDelivery::Healthy,
                DeliveryState::Quiet => CaptureDelivery::Quiet,
                DeliveryState::Recovering => CaptureDelivery::Recovering,
                DeliveryState::Exhausted => CaptureDelivery::Exhausted,
                DeliveryState::CleanupBlocked => CaptureDelivery::CleanupBlocked,
                DeliveryState::Stopped => CaptureDelivery::Stopped,
            },
            attempts: status.attempts,
            max_attempts: status.max_attempts,
            retry_in_ms: status.retry_in_ms,
        };
    }

    /// Queue once, only for an exhausted source. HTTP/session identity checks
    /// belong to the recorder holding this monitor, not to the pump.
    pub fn request_retry(&self, source: CaptureSource) -> bool {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed || inner.retries & bit(source) != 0 {
            return false;
        }
        let Some(health) = inner.health.as_mut() else {
            return false;
        };
        if source_mut(health, source).state != CaptureDelivery::Exhausted {
            return false;
        }
        inner.retries |= bit(source);
        true
    }

    /// Take a queued request at the next pump poll. Stop always wins.
    pub fn take_retry(&self, source: CaptureSource) -> bool {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let pending = !inner.closed && inner.retries & bit(source) != 0;
        inner.retries &= !bit(source);
        pending
    }

    /// Fence updates/retries before stop or finalization releases the session.
    pub fn close(&self) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.closed = true;
        inner.retries = 0;
        if let Some(health) = inner.health.as_mut() {
            for status in [&mut health.system, &mut health.mic] {
                status.state = CaptureDelivery::Stopped;
                status.retry_in_ms = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: DeliveryState) -> RecoveryStatus {
        RecoveryStatus {
            state,
            attempts: 3,
            max_attempts: 3,
            retry_in_ms: None,
        }
    }

    #[test]
    fn dead_microphone_does_not_hide_healthy_system_audio() {
        let monitor = CaptureMonitor::default();
        monitor.publish(CaptureSource::System, status(DeliveryState::Healthy));
        monitor.publish(CaptureSource::Mic, status(DeliveryState::Exhausted));
        let health = monitor.snapshot().unwrap();
        assert_eq!(health.system.state, CaptureDelivery::Healthy);
        assert_eq!(health.mic.state, CaptureDelivery::Exhausted);
        assert!(!monitor.request_retry(CaptureSource::System));
        assert!(monitor.request_retry(CaptureSource::Mic));
        assert!(!monitor.request_retry(CaptureSource::Mic));
        assert!(!monitor.take_retry(CaptureSource::System));
        assert!(monitor.take_retry(CaptureSource::Mic));
        assert!(!monitor.take_retry(CaptureSource::Mic));
    }

    #[test]
    fn close_fences_pending_requests_and_late_publications() {
        let old = CaptureMonitor::default();
        old.publish(CaptureSource::Mic, status(DeliveryState::Exhausted));
        assert!(old.request_retry(CaptureSource::Mic));
        old.close();
        old.publish(CaptureSource::Mic, status(DeliveryState::Healthy));
        assert_eq!(old.snapshot().unwrap().mic.state, CaptureDelivery::Stopped);
        assert!(!old.take_retry(CaptureSource::Mic));
        assert!(!old.request_retry(CaptureSource::Mic));
        let new = CaptureMonitor::default();
        new.publish(CaptureSource::Mic, status(DeliveryState::Exhausted));
        assert!(!new.take_retry(CaptureSource::Mic));
        assert!(new.request_retry(CaptureSource::Mic));
    }
    #[test]
    fn retained_native_cleanup_is_visible_and_cannot_queue_a_retry() {
        let monitor = CaptureMonitor::default();
        monitor.publish(CaptureSource::System, status(DeliveryState::Healthy));
        monitor.publish(CaptureSource::Mic, status(DeliveryState::CleanupBlocked));
        assert_eq!(
            monitor.snapshot().unwrap().mic.state,
            CaptureDelivery::CleanupBlocked
        );
        assert_eq!(
            monitor.snapshot().unwrap().system.state,
            CaptureDelivery::Healthy
        );
        assert!(!monitor.request_retry(CaptureSource::Mic));
        assert!(!monitor.take_retry(CaptureSource::Mic));
    }
}
