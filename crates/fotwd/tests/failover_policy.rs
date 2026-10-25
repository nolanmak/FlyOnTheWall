//! Failure policy uses an injected monotonic clock and VAD speech counter.
use fotw_stt::{StreamState, SttError, SttErrorClass as Class};
use fotwd::failover::coverage::SwitchReason;
use fotwd::failover::policy::{Decision, HandoffLimits, HandoffPolicy};

fn error(class: Class) -> SttError {
    SttError::from_class(class, "fixture-cloud")
}

#[test]
fn credentials_and_quota_demote_immediately_and_never_return_to_cloud() {
    for class in [Class::Auth, Class::Quota] {
        let mut policy = HandoffPolicy::new(HandoffLimits::default());
        let decision = policy.error(12, &error(class));
        assert_eq!(
            decision,
            Decision::Switch(if class == Class::Auth {
                SwitchReason::Auth
            } else {
                SwitchReason::Quota
            })
        );
        policy.state(13, StreamState::Open);
        assert_eq!(policy.tick(14, 0), decision);
    }
}

#[test]
fn counts_failed_reconnect_cycles_instead_of_the_initial_disconnect() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    assert_eq!(policy.error(0, &error(Class::Network)), Decision::KeepCloud);
    for cycle in 1..=3 {
        policy.state(cycle * 100, StreamState::Reconnecting);
        // A socket that opens and instantly dies has not recovered.
        policy.state(cycle * 100 + 1, StreamState::Open);
        let result = policy.error(cycle * 100 + 2, &error(Class::Network));
        assert_eq!(
            result,
            if cycle < 3 {
                Decision::KeepCloud
            } else {
                Decision::Switch(SwitchReason::TransportBudget)
            }
        );
    }
}

#[test]
fn elapsed_retry_budget_is_independent_of_failed_attempt_count() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    policy.error(40_000, &error(Class::Network));
    assert_eq!(policy.tick(54_999, 0), Decision::KeepCloud);
    assert_eq!(
        policy.tick(55_000, 0),
        Decision::Switch(SwitchReason::TransportBudget)
    );
}

#[test]
fn sustained_open_connection_recovers_while_flapping_does_not_reset_the_budget() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    policy.error(0, &error(Class::Network));
    policy.state(10, StreamState::Reconnecting);
    policy.state(100, StreamState::Open);
    assert_eq!(policy.tick(3_100, 0), Decision::KeepCloud);
    assert_eq!(policy.tick(99_000, 0), Decision::KeepCloud);
    assert_eq!(
        policy.error(100_000, &error(Class::Network)),
        Decision::KeepCloud
    );
    assert_eq!(
        policy.tick(115_000, 0),
        Decision::Switch(SwitchReason::TransportBudget)
    );
}

#[test]
fn silence_does_not_trigger_the_no_final_deadline_but_confirmed_speech_does() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    assert_eq!(policy.tick(100_000, 0), Decision::KeepCloud);
    assert_eq!(policy.tick(200_000, 45 * 16_000 - 1), Decision::KeepCloud);
    assert_eq!(
        policy.tick(200_001, 45 * 16_000),
        Decision::Switch(SwitchReason::SpeechStall)
    );
}

#[test]
fn confirmed_final_resets_speech_deadline_and_session_limit_is_not_a_failure() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    policy.confirmed_final(44 * 16_000);
    assert_eq!(policy.tick(100_000, 88 * 16_000), Decision::KeepCloud);
    for time in [100_001, 200_000, 300_000] {
        assert_eq!(
            policy.error(time, &error(Class::SessionLimit)),
            Decision::KeepCloud
        );
        policy.state(time + 1, StreamState::Reconnecting);
    }
    assert_eq!(policy.tick(400_000, 88 * 16_000), Decision::KeepCloud);
}

#[test]
fn rate_pressure_honors_retry_after_within_a_bounded_wait_budget() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    assert_eq!(
        policy.error(100, &error(Class::RateLimit).with_retry_after_ms(10_000)),
        Decision::KeepCloud
    );
    assert_eq!(policy.tick(15_099, 0), Decision::KeepCloud);
    assert_eq!(
        policy.tick(15_100, 0),
        Decision::Switch(SwitchReason::RateLimitBudget)
    );
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    assert_eq!(
        policy.error(0, &error(Class::Concurrency).with_retry_after_ms(120_000)),
        Decision::Switch(SwitchReason::RateLimitBudget)
    );
}

#[test]
fn request_bugs_are_surfaced_instead_of_being_hidden_by_a_provider_switch() {
    for class in [Class::BadRequest, Class::Unsupported, Class::AudioFormat] {
        let mut policy = HandoffPolicy::new(HandoffLimits::default());
        policy.error(0, &error(Class::Network));
        assert_eq!(policy.error(1, &error(class)), Decision::Surface);
        assert_eq!(policy.tick(1_000_000, 99 * 16_000), Decision::Surface);
    }
}

#[test]
fn one_malformed_message_on_an_open_silent_socket_does_not_force_a_switch() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    policy.state(0, StreamState::Open);
    assert_eq!(
        policy.error(100, &error(Class::Server)),
        Decision::KeepCloud
    );
    assert_eq!(policy.tick(3_100, 0), Decision::KeepCloud);
    assert_eq!(policy.tick(100_000, 0), Decision::KeepCloud);
}

#[test]
fn multiple_errors_during_one_reconnect_are_one_failed_cycle() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    policy.error(0, &error(Class::Network));
    policy.state(10, StreamState::Reconnecting);
    for time in [20, 30, 40] {
        assert_eq!(
            policy.error(time, &error(Class::Network)),
            Decision::KeepCloud
        );
    }
}

#[test]
fn an_initial_connection_is_bounded_and_a_healthy_initial_open_clears_it() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    policy.state(100, StreamState::Connecting);
    assert_eq!(policy.tick(15_099, 0), Decision::KeepCloud);
    assert_eq!(
        policy.tick(15_100, 0),
        Decision::Switch(SwitchReason::TransportBudget)
    );
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    policy.state(0, StreamState::Connecting);
    policy.state(100, StreamState::Open);
    assert_eq!(policy.tick(3_100, 0), Decision::KeepCloud);
    assert_eq!(policy.tick(100_000, 0), Decision::KeepCloud);
}

#[test]
fn configured_budgets_replace_the_defaults() {
    let limits = HandoffLimits {
        failed_cycles: 1,
        retry_budget_ms: 200,
        speech_samples_without_final: 1_600,
        healthy_open_ms: 50,
        rate_pressure_budget_ms: 300,
    };
    let mut policy = HandoffPolicy::new(limits);
    policy.error(0, &error(Class::Network));
    assert_eq!(policy.tick(199, 0), Decision::KeepCloud);
    assert_eq!(
        policy.tick(200, 0),
        Decision::Switch(SwitchReason::TransportBudget)
    );
    let mut policy = HandoffPolicy::new(limits);
    assert_eq!(policy.tick(0, 1_599), Decision::KeepCloud);
    assert_eq!(
        policy.tick(1, 1_600),
        Decision::Switch(SwitchReason::SpeechStall)
    );
}

#[test]
fn pressure_never_extends_the_fifteen_second_retry_budget() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    assert_eq!(
        policy.error(100, &error(Class::RateLimit).with_retry_after_ms(10_000)),
        Decision::KeepCloud
    );
    assert_eq!(policy.tick(15_099, 0), Decision::KeepCloud);
    assert_eq!(
        policy.tick(15_100, 0),
        Decision::Switch(SwitchReason::RateLimitBudget)
    );
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    assert_eq!(
        policy.error(100, &error(Class::Concurrency).with_retry_after_ms(20_000)),
        Decision::Switch(SwitchReason::RateLimitBudget)
    );
}

#[test]
fn repeated_pressure_uses_remaining_retry_time_instead_of_restarting_the_budget() {
    let mut policy = HandoffPolicy::new(HandoffLimits::default());
    policy.state(0, StreamState::Connecting);
    policy.error(0, &error(Class::Network));
    assert_eq!(
        policy.error(10_000, &error(Class::RateLimit).with_retry_after_ms(8_000)),
        Decision::Switch(SwitchReason::RateLimitBudget)
    );
}
