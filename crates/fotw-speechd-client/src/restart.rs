//! The helper's restart budget, kept pure: callers pass the clock in.

use std::time::{Duration, Instant};

/// How the client restarts a helper that died.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Restarts in a row before giving up. A stream that completes resets it.
    pub max_attempts: u32,
    /// Least time between two spawns.
    pub min_interval: Duration,
}

impl Default for RestartPolicy {
    /// 3 attempts, at least 2 s apart (#243).
    fn default() -> Self {
        Self {
            max_attempts: 3,
            min_interval: Duration::from_secs(2),
        }
    }
}

/// Counts restarts since the last success and spaces them out.
#[derive(Debug, Clone)]
pub struct RestartBudget {
    policy: RestartPolicy,
    attempts: u32,
    last_spawn: Option<Instant>,
}

/// The budget is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhausted {
    /// Attempts made.
    pub attempts: u32,
}

impl RestartBudget {
    /// A fresh budget.
    #[must_use]
    pub fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            attempts: 0,
            last_spawn: None,
        }
    }

    /// Take one attempt, returning how long to wait before spawning.
    ///
    /// # Errors
    /// [`Exhausted`] once `max_attempts` restarts have been taken without a
    /// success in between.
    pub fn take(&mut self, now: Instant) -> Result<Duration, Exhausted> {
        if self.attempts >= self.policy.max_attempts {
            return Err(Exhausted {
                attempts: self.attempts,
            });
        }
        self.attempts += 1;
        Ok(self.last_spawn.map_or(Duration::ZERO, |last| {
            (last + self.policy.min_interval).saturating_duration_since(now)
        }))
    }

    /// Record a spawn, which the next attempt is spaced from.
    pub fn spawned(&mut self, at: Instant) {
        self.last_spawn = Some(at);
    }

    /// Something worked: the next failure starts a fresh run of attempts.
    pub fn succeeded(&mut self) {
        self.attempts = 0;
    }

    /// Restarts taken since the last success.
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_attempts_each_at_least_two_seconds_after_the_last_spawn() {
        let t0 = Instant::now();
        let mut budget = RestartBudget::new(RestartPolicy::default());
        budget.spawned(t0);
        // Died 300 ms after spawning: wait out the rest of the 2 s.
        assert_eq!(
            budget.take(t0 + Duration::from_millis(300)),
            Ok(Duration::from_millis(1700))
        );
        budget.spawned(t0 + Duration::from_secs(2));
        // Died long after: no wait.
        assert_eq!(
            budget.take(t0 + Duration::from_secs(10)),
            Ok(Duration::ZERO)
        );
        budget.spawned(t0 + Duration::from_secs(10));
        assert_eq!(
            budget.take(t0 + Duration::from_secs(10)),
            Ok(Duration::from_secs(2))
        );
        budget.spawned(t0 + Duration::from_secs(12));
        assert_eq!(
            budget.take(t0 + Duration::from_secs(20)),
            Err(Exhausted { attempts: 3 })
        );
    }

    #[test]
    fn a_success_resets_the_count_but_not_the_spacing() {
        let t0 = Instant::now();
        let mut budget = RestartBudget::new(RestartPolicy::default());
        budget.spawned(t0);
        for _ in 0..3 {
            budget.take(t0).unwrap();
        }
        assert!(budget.take(t0).is_err());
        budget.succeeded();
        assert_eq!(
            budget.take(t0 + Duration::from_millis(500)),
            Ok(Duration::from_millis(1500))
        );
        assert_eq!(budget.attempts(), 1);
    }
}
