//! Numerical timing gates for local-summary qualification (#236).
//!
//! This validates completeness, identity binding and timing arithmetic in an
//! externally authenticated measurement report. It cannot authenticate a
//! machine, attest how measurements were collected, certify task quality or
//! produce a model recommendation. Resource/semantic/journey gates are separate.
use serde::{Deserialize, Serialize};

/// Timing limits frozen before collecting any candidate trials.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenLimits {
    /// Full model/runtime/prompt/tokenizer/language/context/decoding identity.
    pub configuration_sha256: String,
    /// Timestamp in the measurement report's declared clock, milliseconds.
    pub frozen_at_ms: u64,
    /// Maximum individual process-cold model load time.
    pub cold_load_ms: u64,
    /// Nearest-rank warm p95 for complete ten-minute jobs.
    pub ten_minute_p95_ms: u64,
    /// Nearest-rank warm p95 for complete sixty-minute jobs.
    pub sixty_minute_p95_ms: u64,
}
impl FrozenLimits {
    /// Stable content identity; this is not a signature or proof of chronology.
    pub fn digest(&self) -> String {
        crate::hash::sha256_hex(&serde_json::to_vec(self).expect("timing limits serialize"))
    }
}
/// Origin claimed by the report producer; authenticate the receipt separately.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Origin {
    /// Physical machine metadata with an independently auditable receipt.
    Physical {
        /// Digest of the physical-machine/collection receipt.
        receipt_sha256: String,
    },
    /// Injected/capped hardware does not establish physical-machine evidence.
    Simulated,
    /// The measurement source is absent or not established.
    Unknown,
}
/// Trial classification; process-cold does not mean flushed filesystem cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Complete pipeline and persistence for a ten-minute fixture.
    WarmTenMinute,
    /// Complete pipeline and persistence for a sixty-minute fixture.
    WarmSixtyMinute,
    /// A separately started model/runtime process.
    ProcessCold,
}
/// Every attempted trial remains in the report, including failures.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Completion {
    /// Finished complete-job timing, not only one model generation.
    Completed {
        /// Includes chunking, retries, notes and result persistence.
        total_ms: u64,
        /// Required on process-cold trials; separate from total job time.
        load_ms: Option<u64>,
    },
    /// A declared deadline elapsed; never drop this from the denominator.
    TimedOut,
    /// The attempted job failed, including malformed/unpersisted results.
    Failed,
    /// Collection did not establish a completed or failed outcome.
    Unverified,
}
/// One attempted measurement, bound to its original frozen configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trial {
    /// Unique attempt identity; duplicate rows cannot inflate trial counts.
    pub id: String,
    /// Immutable configuration that actually ran.
    pub configuration_sha256: String,
    /// Original limits digest captured when this trial began.
    pub limits_sha256: String,
    /// Start timestamp in the same declared clock as `frozen_at_ms`.
    pub started_at_ms: u64,
    /// Warm fixture class or a verified process-cold attempt.
    pub kind: Kind,
    /// Unique per-load evidence the model/runtime was absent before this
    /// process-cold load. This is not evidence of an OS disk-cache flush.
    pub process_cold_receipt_sha256: Option<String>,
    /// Complete outcome, including unsuccessful attempts.
    pub completion: Completion,
}
/// Input to numerical gates; no model downloads, probes or inference occur.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// Version of this numerical report contract; currently 1.
    pub format_version: u32,
    /// Receipt provenance, authenticated outside this arithmetic layer.
    pub origin: Origin,
    /// Observed physical RAM, not a requested memory limit.
    pub physical_bytes: Option<u64>,
    /// Frozen per-configuration limits; reference profiles cannot be relaxed.
    pub limits: FrozenLimits,
    /// All attempted measurements for this exact configuration.
    pub trials: Vec<Trial>,
}
/// Numerical timing result only; never a quality/hardware certification badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Complete, consistently identified numbers meet the timing gates.
    WithinLimits,
    /// At least one known failure, timeout or timing threshold violation.
    Failed,
    /// Missing, simulated, inconsistent or insufficient measurement evidence.
    Unverified,
}
/// A content-free diagnostic. Trial IDs are producer-controlled metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Issue {
    /// This numerical report contract version is not understood.
    UnsupportedVersion,
    /// Receipt provenance or physical RAM is missing/not physical.
    PhysicalEvidenceMissing,
    /// Limits are malformed, or relax a fixed low-memory reference gate.
    InvalidLimits,
    /// Duplicate/empty attempt identifier or mismatched immutable identity.
    InvalidTrialIdentity,
    /// Collection began before these limits were frozen.
    LimitsNotFrozenBeforeTrial,
    /// Unknown/contradictory process-cold classification or load measurement.
    ColdEvidenceMissing,
    /// Fewer than twenty successful warm jobs in a required fixture class.
    WarmTrialsMissing,
    /// Fewer than five successful process-cold trials.
    ColdTrialsMissing,
    /// An attempted trial failed or timed out.
    TrialFailed,
    /// Outcome collection did not establish success or failure.
    TrialUnverified,
    /// A warm fixture class exceeded its nearest-rank p95 gate.
    WarmDeadlineExceeded,
    /// Any individual process-cold load exceeded its frozen deadline.
    ColdDeadlineExceeded,
}
/// Evaluated numerical evidence. Missing percentiles remain missing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    /// Timing-only status; all other qualification gates remain separate.
    pub status: Status,
    /// Warm ten-minute nearest-rank p95, when completed observations exist.
    pub ten_minute_p95_ms: Option<u64>,
    /// Warm sixty-minute nearest-rank p95, when completed observations exist.
    pub sixty_minute_p95_ms: Option<u64>,
    /// Reasons this report cannot pass the timing-only gate.
    pub issues: Vec<Issue>,
}
/// Nearest-rank p95: sorted observation at ceil(0.95 * n), one-based.
/// Empty input has no percentile; zero elapsed milliseconds is an observation.
pub fn nearest_rank_p95(observations: &[u64]) -> Option<u64> {
    if observations.is_empty() {
        return None;
    }
    let mut sorted = observations.to_vec();
    sorted.sort_unstable();
    // ceil(19*n/20) = n - floor(n/20), without overflowing n*95.
    Some(sorted[sorted.len() - sorted.len() / 20 - 1])
}
/// Check complete timing records without certifying report authenticity or
/// semantic/resource correctness. No result here constructs `QualityEvidence`.
pub fn evaluate(report: &Report) -> Evaluation {
    use crate::local_model::GIB;
    let mut issues = Vec::new();
    let mut note = |issue| {
        if !issues.contains(&issue) {
            issues.push(issue);
        }
    };
    if report.format_version != 1 {
        note(Issue::UnsupportedVersion);
    }
    if !matches!(&report.origin, Origin::Physical { receipt_sha256 } if digest(receipt_sha256))
        || report.physical_bytes.is_none_or(|bytes| bytes == 0)
    {
        note(Issue::PhysicalEvidenceMissing);
    }
    let limits = &report.limits;
    let specified = (
        limits.cold_load_ms,
        limits.ten_minute_p95_ms,
        limits.sixty_minute_p95_ms,
    );
    let reference = match report.physical_bytes {
        Some(bytes) if bytes == 8 * GIB => Some((60_000, 300_000, 900_000)),
        Some(bytes) if bytes == 16 * GIB => Some((60_000, 180_000, 600_000)),
        Some(bytes) if bytes == 24 * GIB => Some((60_000, 120_000, 480_000)),
        _ => None,
    };
    let limits_valid = digest(&limits.configuration_sha256)
        && specified.0 > 0
        && specified.1 > 0
        && specified.2 > 0
        && reference.is_none_or(|fixed| fixed == specified);
    if !limits_valid {
        note(Issue::InvalidLimits);
    }
    let limits_digest = limits.digest();
    let mut ids = std::collections::HashSet::new();
    let mut cold_receipts = std::collections::HashSet::new();
    let (mut ten, mut sixty, mut cold) = (Vec::new(), Vec::new(), 0_usize);
    for trial in &report.trials {
        // Record unsuccessful attempts even if other report fields are missing
        // or inconsistent. They must not disappear into a successful-only p95.
        match trial.completion {
            Completion::Failed | Completion::TimedOut => note(Issue::TrialFailed),
            Completion::Unverified => note(Issue::TrialUnverified),
            Completion::Completed { .. } => {}
        }
        let mut valid = true;
        if trial.id.trim().is_empty()
            || !ids.insert(&trial.id)
            || trial.configuration_sha256 != limits.configuration_sha256
            || trial.limits_sha256 != limits_digest
        {
            note(Issue::InvalidTrialIdentity);
            valid = false;
        }
        if trial.started_at_ms < limits.frozen_at_ms {
            note(Issue::LimitsNotFrozenBeforeTrial);
            valid = false;
        }
        let Completion::Completed { total_ms, load_ms } = trial.completion else {
            continue;
        };
        match trial.kind {
            Kind::ProcessCold => {
                let verified = trial
                    .process_cold_receipt_sha256
                    .as_deref()
                    .is_some_and(|receipt| digest(receipt) && cold_receipts.insert(receipt))
                    && load_ms.is_some_and(|load| load <= total_ms);
                if !verified {
                    note(Issue::ColdEvidenceMissing);
                    valid = false;
                }
                if limits_valid && valid && load_ms.is_some_and(|load| load > limits.cold_load_ms) {
                    note(Issue::ColdDeadlineExceeded);
                }
                if valid {
                    cold += 1;
                }
            }
            Kind::WarmTenMinute | Kind::WarmSixtyMinute => {
                if load_ms.is_some() || trial.process_cold_receipt_sha256.is_some() {
                    note(Issue::ColdEvidenceMissing);
                    valid = false;
                }
                if valid {
                    if trial.kind == Kind::WarmTenMinute {
                        ten.push(total_ms);
                    } else {
                        sixty.push(total_ms);
                    }
                }
            }
        }
    }
    if ten.len() < 20 || sixty.len() < 20 {
        note(Issue::WarmTrialsMissing);
    }
    if cold < 5 {
        note(Issue::ColdTrialsMissing);
    }
    let ten_minute_p95_ms = nearest_rank_p95(&ten);
    let sixty_minute_p95_ms = nearest_rank_p95(&sixty);
    if limits_valid
        && (ten_minute_p95_ms.is_some_and(|time| time > limits.ten_minute_p95_ms)
            || sixty_minute_p95_ms.is_some_and(|time| time > limits.sixty_minute_p95_ms))
    {
        note(Issue::WarmDeadlineExceeded);
    }
    let status = if issues.iter().any(|issue| {
        matches!(
            issue,
            Issue::TrialFailed | Issue::WarmDeadlineExceeded | Issue::ColdDeadlineExceeded
        )
    }) {
        Status::Failed
    } else if issues.is_empty() {
        Status::WithinLimits
    } else {
        Status::Unverified
    };
    Evaluation {
        status,
        ten_minute_p95_ms,
        sixty_minute_p95_ms,
        issues,
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::GIB;
    fn report() -> Report {
        let limits = FrozenLimits {
            configuration_sha256: "a".repeat(64),
            frozen_at_ms: 10,
            cold_load_ms: 60_000,
            ten_minute_p95_ms: 300_000,
            sixty_minute_p95_ms: 900_000,
        };
        let mut trials = Vec::new();
        for (kind, count) in [
            (Kind::WarmTenMinute, 20),
            (Kind::WarmSixtyMinute, 20),
            (Kind::ProcessCold, 5),
        ] {
            for i in 0..count {
                trials.push(Trial {
                    id: format!("{kind:?}-{i}"),
                    configuration_sha256: limits.configuration_sha256.clone(),
                    limits_sha256: limits.digest(),
                    started_at_ms: 10 + i,
                    kind,
                    process_cold_receipt_sha256: (kind == Kind::ProcessCold)
                        .then(|| crate::hash::sha256_hex(format!("process-cold-{i}").as_bytes())),
                    completion: Completion::Completed {
                        total_ms: 1000 + i,
                        load_ms: (kind == Kind::ProcessCold).then_some(500),
                    },
                });
            }
        }
        Report {
            format_version: 1,
            origin: Origin::Physical {
                receipt_sha256: "b".repeat(64),
            },
            physical_bytes: Some(8 * GIB),
            limits,
            trials,
        }
    }
    #[test]
    fn percentiles_use_nearest_rank_without_interpolation_or_empty_zero() {
        assert_eq!(nearest_rank_p95(&[]), None);
        assert_eq!(nearest_rank_p95(&[0]), Some(0));
        assert_eq!(
            nearest_rank_p95(&(1..=20).rev().collect::<Vec<_>>()),
            Some(19)
        );
        assert_eq!(nearest_rank_p95(&(1..=21).collect::<Vec<_>>()), Some(20));
    }
    #[test]
    fn complete_reference_numbers_meet_only_the_timing_gate() {
        let e = evaluate(&report());
        assert_eq!(e.status, Status::WithinLimits);
        assert_eq!(e.ten_minute_p95_ms, Some(1018));
        assert_eq!(e.sixty_minute_p95_ms, Some(1018));
        assert!(e.issues.is_empty());
    }
    #[test]
    fn incomplete_or_simulated_evidence_never_passes() {
        for kind in 0..4 {
            let mut r = report();
            match kind {
                0 => r.origin = Origin::Simulated,
                1 => r.physical_bytes = None,
                2 => {
                    r.trials.remove(0);
                }
                _ => {
                    r.trials.pop();
                }
            }
            assert_eq!(evaluate(&r).status, Status::Unverified, "case {kind}");
        }
    }
    #[test]
    fn timeouts_and_failures_are_retained_even_when_other_evidence_is_missing() {
        for failure in [Completion::TimedOut, Completion::Failed] {
            let mut r = report();
            r.origin = Origin::Unknown;
            r.trials[0].completion = failure;
            assert_eq!(evaluate(&r).status, Status::Failed);
            assert!(evaluate(&r).issues.contains(&Issue::TrialFailed));
        }
    }
    #[test]
    fn every_cold_load_must_meet_its_limit_even_when_p95_would_hide_one() {
        let mut r = report();
        r.trials.last_mut().unwrap().completion = Completion::Completed {
            total_ms: 61_000,
            load_ms: Some(60_001),
        };
        assert_eq!(evaluate(&r).status, Status::Failed);
        assert!(evaluate(&r).issues.contains(&Issue::ColdDeadlineExceeded));
    }
    #[test]
    fn warm_deadlines_use_the_correct_fixture_class_and_inclusive_boundary() {
        let mut r = report();
        for t in &mut r.trials {
            if t.kind == Kind::WarmSixtyMinute {
                t.completion = Completion::Completed {
                    total_ms: 900_000,
                    load_ms: None,
                };
            }
        }
        assert_eq!(evaluate(&r).status, Status::WithinLimits);
        for t in &mut r.trials {
            if t.kind == Kind::WarmSixtyMinute {
                t.completion = Completion::Completed {
                    total_ms: 900_001,
                    load_ms: None,
                };
            }
        }
        assert_eq!(evaluate(&r).status, Status::Failed);
        assert!(evaluate(&r).issues.contains(&Issue::WarmDeadlineExceeded));
    }
    #[test]
    fn duplicate_trials_and_retroactive_limit_changes_cannot_inflate_evidence() {
        let mut r = report();
        r.trials[1].id = r.trials[0].id.clone();
        let e = evaluate(&r);
        assert_eq!(e.status, Status::Unverified);
        assert!(e.issues.contains(&Issue::InvalidTrialIdentity));
        let mut r = report();
        r.limits.frozen_at_ms += 100;
        let e = evaluate(&r);
        assert_eq!(e.status, Status::Unverified);
        assert!(e.issues.contains(&Issue::LimitsNotFrozenBeforeTrial));
    }
    #[test]
    fn cold_classification_needs_its_own_process_absence_receipt() {
        let mut r = report();
        r.trials.last_mut().unwrap().process_cold_receipt_sha256 = None;
        let e = evaluate(&r);
        assert_eq!(e.status, Status::Unverified);
        assert!(e.issues.contains(&Issue::ColdEvidenceMissing));
    }
    #[test]
    fn large_configuration_deadlines_are_predeclared_without_a_laptop_ceiling() {
        let mut r = report();
        r.physical_bytes = Some(512 * GIB);
        r.limits.cold_load_ms = 600_000;
        r.limits.ten_minute_p95_ms = 1_200_000;
        r.limits.sixty_minute_p95_ms = 3_600_000;
        let digest = r.limits.digest();
        for t in &mut r.trials {
            t.limits_sha256 = digest.clone();
            t.completion = Completion::Completed {
                total_ms: 1_000_000,
                load_ms: (t.kind == Kind::ProcessCold).then_some(300_000),
            };
        }
        assert_eq!(evaluate(&r).status, Status::WithinLimits);
        r.physical_bytes = Some(8 * GIB);
        let e = evaluate(&r);
        assert_eq!(e.status, Status::Unverified);
        assert!(
            e.issues.contains(&Issue::InvalidLimits),
            "reference gates cannot be relaxed"
        );
    }
    #[test]
    fn one_cold_process_receipt_cannot_be_reused_as_five_separate_loads() {
        let mut r = report();
        let receipt = r.trials.last().unwrap().process_cold_receipt_sha256.clone();
        for t in &mut r.trials {
            if t.kind == Kind::ProcessCold {
                t.process_cold_receipt_sha256 = receipt.clone();
            }
        }
        let e = evaluate(&r);
        assert_eq!(e.status, Status::Unverified);
        assert!(e.issues.contains(&Issue::ColdEvidenceMissing));
    }
    #[test]
    fn malformed_or_unknown_measurement_records_stay_unverified() {
        for case in 0..8 {
            let mut r = report();
            let expected = match case {
                0 => {
                    r.format_version = 2;
                    Issue::UnsupportedVersion
                }
                1 => {
                    r.limits.configuration_sha256 = "not-a-digest".into();
                    Issue::InvalidLimits
                }
                2 => {
                    r.origin = Origin::Physical {
                        receipt_sha256: String::new(),
                    };
                    Issue::PhysicalEvidenceMissing
                }
                3 => {
                    r.trials[0].completion = Completion::Unverified;
                    Issue::TrialUnverified
                }
                4 => {
                    r.trials[0].configuration_sha256 = "d".repeat(64);
                    Issue::InvalidTrialIdentity
                }
                5 => {
                    r.trials.last_mut().unwrap().completion = Completion::Completed {
                        total_ms: 1000,
                        load_ms: None,
                    };
                    Issue::ColdEvidenceMissing
                }
                6 => {
                    r.trials.last_mut().unwrap().completion = Completion::Completed {
                        total_ms: 100,
                        load_ms: Some(500),
                    };
                    Issue::ColdEvidenceMissing
                }
                _ => {
                    r.trials[0].process_cold_receipt_sha256 = Some("c".repeat(64));
                    Issue::ColdEvidenceMissing
                }
            };
            let e = evaluate(&r);
            assert_eq!(e.status, Status::Unverified, "case {case}: {e:?}");
            assert!(e.issues.contains(&expected), "case {case}: {e:?}");
        }
    }
    #[test]
    fn other_reference_profiles_keep_their_exact_predeclared_limits() {
        for (gib, ten, sixty) in [(16, 180_000, 600_000), (24, 120_000, 480_000)] {
            let mut r = report();
            r.physical_bytes = Some(gib * GIB);
            r.limits.ten_minute_p95_ms = ten;
            r.limits.sixty_minute_p95_ms = sixty;
            let digest = r.limits.digest();
            for t in &mut r.trials {
                t.limits_sha256 = digest.clone();
            }
            assert_eq!(evaluate(&r).status, Status::WithinLimits);
            r.limits.ten_minute_p95_ms += 1;
            assert_eq!(evaluate(&r).status, Status::Unverified);
        }
    }
    #[test]
    fn invalid_zero_deadlines_do_not_turn_missing_evidence_into_a_model_failure() {
        let mut r = report();
        r.limits.cold_load_ms = 0;
        let e = evaluate(&r);
        assert_eq!(e.status, Status::Unverified);
        assert!(e.issues.contains(&Issue::InvalidLimits));
        assert!(!e.issues.contains(&Issue::ColdDeadlineExceeded));
    }
}
