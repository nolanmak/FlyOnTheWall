//! Recording deadlines must also expire across laptop sleep.
//!
//! Keep the monotonic timer for clock corrections, and check wall time every
//! second for suspend/resume. Neither requires a browser tab to stay open.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) async fn wait(duration: Duration, started_at_ms: u64) {
    wait_with_clock(duration, started_at_ms, || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    })
    .await;
}

async fn wait_with_clock(duration: Duration, started_at_ms: u64, now: impl Fn() -> u64) {
    let limit_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    let wall_deadline = async {
        loop {
            let elapsed = now().saturating_sub(started_at_ms);
            if elapsed >= limit_ms {
                return;
            }
            tokio::time::sleep(Duration::from_millis((limit_ms - elapsed).min(1000))).await;
        }
    };
    tokio::select! {
        () = tokio::time::sleep(duration) => {}
        () = wall_deadline => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[tokio::test]
    async fn waking_past_the_deadline_does_not_wait_for_uptime() {
        let now = AtomicU64::new(1_000);
        let deadline = wait_with_clock(Duration::from_secs(7200), 1_000, || {
            now.load(Ordering::Relaxed)
        });
        let wake = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            now.store(1_000 + 23 * 60 * 60 * 1000, Ordering::Relaxed);
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(deadline, wake);
        })
        .await
        .expect("resume must expire a two-hour limit within one polling interval");
    }

    #[tokio::test]
    async fn a_clock_set_back_cannot_extend_the_monotonic_limit() {
        tokio::time::timeout(
            Duration::from_secs(1),
            wait_with_clock(Duration::from_millis(40), 10_000, || 1_000),
        )
        .await
        .expect("a backwards wall clock must not prevent auto-stop");
    }

    #[tokio::test]
    async fn a_future_deadline_does_not_stop_immediately() {
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                wait_with_clock(Duration::from_secs(10), 1_000, || 1_100),
            )
            .await
            .is_err()
        );
    }
}
