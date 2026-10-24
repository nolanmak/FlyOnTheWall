//! Per-source cloud health, driven by an injected monotonic clock and VAD
//! speech-sample counter. It makes no reachability probes or wall-clock reads.

use fotw_stt::{StreamState, SttError, SttErrorClass};

use super::coverage::SwitchReason;

/// Configurable budgets for an explicitly enabled cloud-to-local policy.
#[derive(Debug, Clone, Copy)]
pub struct HandoffLimits {
    /// Failed reconnect cycles; the initial disconnect does not count.
    pub failed_cycles: u32,
    /// Longest transport retry episode, in monotonic milliseconds.
    pub retry_budget_ms: u64,
    /// VAD-confirmed speech samples without a provider-confirmed final.
    pub speech_samples_without_final: u64,
    /// An open socket must stay healthy this long to end a retry episode.
    pub healthy_open_ms: u64,
    /// Maximum wait for rate/concurrency pressure, including Retry-After.
    pub rate_pressure_budget_ms: u64,
}

impl Default for HandoffLimits {
    fn default() -> Self {
        Self {
            failed_cycles: 3,
            retry_budget_ms: 15_000,
            speech_samples_without_final: 45 * 16_000,
            healthy_open_ms: 3_000,
            rate_pressure_budget_ms: 30_000,
        }
    }
}

/// What the handoff worker should do with the cloud leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Keep the cloud stream while its own transport obeys retry/backoff.
    KeepCloud,
    /// Demote to local for the remainder of this meeting.
    Switch(SwitchReason),
    /// Surface an application/request failure; do not mask it with a switch.
    Surface,
}

/// One source's policy. A demotion is sticky even when Wi-Fi starts flapping.
pub struct HandoffPolicy {
    limits: HandoffLimits,
    decision: Decision,
    transport_since: Option<u64>,
    pressure_since: Option<u64>,
    recovering: bool,
    cycle_armed: bool,
    last_state: Option<StreamState>,
    failed_cycles: u32,
    opened_since: Option<u64>,
    speech_at_final: u64,
}

impl HandoffPolicy {
    /// Start at source admission. `now_ms` inputs must share one monotonic clock.
    #[must_use]
    pub fn new(limits: HandoffLimits) -> Self {
        Self {
            limits,
            decision: Decision::KeepCloud,
            transport_since: None,
            pressure_since: None,
            recovering: false,
            cycle_armed: false,
            last_state: None,
            failed_cycles: 0,
            opened_since: None,
            speech_at_final: 0,
        }
    }

    /// Observe a provider error. The worker still reports the original error;
    /// this decision only controls automatic handoff, never error visibility.
    pub fn error(&mut self, now_ms: u64, error: &SttError) -> Decision {
        if self.decision != Decision::KeepCloud {
            return self.decision;
        }
        match error.class {
            SttErrorClass::Auth => self.decision = Decision::Switch(SwitchReason::Auth),
            SttErrorClass::Quota => self.decision = Decision::Switch(SwitchReason::Quota),
            SttErrorClass::SessionLimit => {}
            SttErrorClass::BadRequest | SttErrorClass::Unsupported | SttErrorClass::AudioFormat => {
                self.decision = Decision::Surface;
            }
            SttErrorClass::Network | SttErrorClass::Server => {
                // Malformed provider messages can surface a Server error
                // without closing the socket. A sustained open socket ends
                // that episode; an actual reconnect clears this health timer.
                self.opened_since = (error.class == SttErrorClass::Server
                    && self.last_state == Some(StreamState::Open))
                .then_some(now_ms);
                self.transport_since.get_or_insert(now_ms);
                if self.cycle_armed {
                    self.cycle_armed = false;
                    self.failed_cycles = self.failed_cycles.saturating_add(1);
                    if self.failed_cycles >= self.limits.failed_cycles {
                        self.decision = Decision::Switch(SwitchReason::TransportBudget);
                    }
                }
            }
            SttErrorClass::RateLimit | SttErrorClass::Concurrency => {
                self.opened_since = None;
                self.pressure_since.get_or_insert(now_ms);
                // Do not retry earlier than the server permits. If waiting
                // would exceed this policy's budget, use local immediately.
                if error
                    .retry_after_ms
                    .is_some_and(|wait| wait > self.limits.rate_pressure_budget_ms)
                {
                    self.decision = Decision::Switch(SwitchReason::RateLimitBudget);
                }
            }
        }
        self.tick(now_ms, self.speech_at_final)
    }

    /// Observe transport state. An Open event alone does not forgive a failed
    /// episode: sockets that immediately die are failed reconnect cycles.
    pub fn state(&mut self, now_ms: u64, state: StreamState) {
        if self.decision != Decision::KeepCloud {
            return;
        }
        self.last_state = Some(state);
        match state {
            StreamState::Reconnecting => {
                self.recovering = true;
                self.cycle_armed = true;
                self.opened_since = None;
            }
            StreamState::Open if self.recovering || self.transport_since.is_some() => {
                self.opened_since = Some(now_ms)
            }
            StreamState::Connecting => {
                self.transport_since.get_or_insert(now_ms);
            }
            StreamState::Closed => self.opened_since = None,
            StreamState::Open => {}
        }
    }

    /// A provider-confirmed, durably saved final proves health. The caller must
    /// fence old provider epochs before invoking this method.
    pub fn confirmed_final(&mut self, voiced_samples: u64) {
        if self.decision != Decision::KeepCloud {
            return;
        }
        self.speech_at_final = voiced_samples;
        self.recovered();
    }

    /// Evaluate elapsed retry time and cumulative VAD-confirmed speech. Silence
    /// leaves the speech counter unchanged, regardless of elapsed wall time.
    pub fn tick(&mut self, now_ms: u64, voiced_samples: u64) -> Decision {
        if self.decision != Decision::KeepCloud {
            return self.decision;
        }
        if self
            .opened_since
            .is_some_and(|opened| now_ms.saturating_sub(opened) >= self.limits.healthy_open_ms)
        {
            self.recovered();
        }
        if self
            .transport_since
            .is_some_and(|start| now_ms.saturating_sub(start) >= self.limits.retry_budget_ms)
        {
            self.decision = Decision::Switch(SwitchReason::TransportBudget);
        } else if self.pressure_since.is_some_and(|start| {
            now_ms.saturating_sub(start) >= self.limits.rate_pressure_budget_ms
        }) {
            self.decision = Decision::Switch(SwitchReason::RateLimitBudget);
        } else if voiced_samples.saturating_sub(self.speech_at_final)
            >= self.limits.speech_samples_without_final
        {
            self.decision = Decision::Switch(SwitchReason::SpeechStall);
        }
        self.decision
    }

    fn recovered(&mut self) {
        self.recovering = false;
        self.cycle_armed = false;
        self.failed_cycles = 0;
        self.transport_since = None;
        self.pressure_since = None;
        self.opened_since = None;
    }
}
