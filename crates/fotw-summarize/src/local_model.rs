//! Pure local-summary resource admission and catalog ranking.
//!
//! This layer performs no probing, downloading or inference. A daemon supplies
//! a fresh hardware snapshot and verified catalog entries; arithmetic here
//! does not establish model quality or certify an estimated fit on this Mac.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A binary GiB, used for resource accounting rather than download labels.
pub const GIB: u64 = 1 << 30;

/// Current system memory pressure, independently of installed RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    /// The OS reports no pressure warning.
    Normal,
    /// New summary inference must wait.
    Warning,
    /// New summary inference must wait.
    Critical,
}

/// Input from a current, non-private system probe.
#[derive(Debug, Clone, Copy)]
pub struct HardwareSnapshot {
    /// Whether this is a supported Apple Silicon/macOS runtime target.
    pub supported_platform: bool,
    /// Installed unified memory, in bytes; missing values fail closed.
    pub physical_bytes: Option<u64>,
    /// Available memory excluding the owned resident model's private bytes.
    pub available_bytes: Option<u64>,
    /// Current private bytes owned by this summary runtime, not other apps.
    pub owned_resident_bytes: u64,
    /// Missing pressure information fails closed.
    pub pressure: Option<MemoryPressure>,
}

/// A refusal contains no transcript or provider information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    /// Platform/runtime compatibility must be checked before admission.
    #[error("Local summaries are unsupported on this platform")]
    UnsupportedPlatform,
    /// Do not guess headroom when a probe is missing or inconsistent.
    #[error("Current memory information is unavailable or inconsistent")]
    MemoryUnknown,
    /// Installed RAM does not override current system pressure.
    #[error("Local summaries are waiting for memory pressure to clear")]
    MemoryPressure,
}

/// Maximum total private footprint of the owned runtime at this observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    /// Headroom reserved for the OS and other work.
    pub reserve_bytes: u64,
    /// Total allowed footprint, including an already resident owned model.
    pub runtime_bytes: u64,
}

/// Compute the epic's continuous budget, with a user cap that can reduce it.
/// Integer rounding reserves upward and limits downward, in bytes.
pub fn memory_budget(
    host: HardwareSnapshot,
    user_cap_bytes: Option<u64>,
) -> Result<MemoryBudget, AdmissionError> {
    if !host.supported_platform {
        return Err(AdmissionError::UnsupportedPlatform);
    }
    match host.pressure {
        Some(MemoryPressure::Normal) => {}
        Some(_) => return Err(AdmissionError::MemoryPressure),
        None => return Err(AdmissionError::MemoryUnknown),
    }
    let physical = host.physical_bytes.ok_or(AdmissionError::MemoryUnknown)?;
    let available = host.available_bytes.ok_or(AdmissionError::MemoryUnknown)?;
    // Probe contract: available excludes the resident model's private bytes.
    // They remain available to that same owner's total budget, not a second
    // model. Never add another application's RSS or a reclaimable shared map.
    let reusable = available
        .checked_add(host.owned_resident_bytes)
        .filter(|bytes| physical > 0 && *bytes <= physical)
        .ok_or(AdmissionError::MemoryUnknown)?;
    let reserve_bytes = (2 * GIB).max(physical.div_ceil(10));
    let physical_cap = (u128::from(physical) * 7 / 10) as u64;
    Ok(MemoryBudget {
        reserve_bytes,
        runtime_bytes: physical_cap
            .min(reusable.saturating_sub(reserve_bytes))
            .min(user_cap_bytes.unwrap_or(u64::MAX)),
    })
}

/// User preference; a larger model is not automatically the desired model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preference {
    /// Three parts task-quality score to one part bounded speed score.
    #[default]
    Balanced,
    /// Prefer lower predicted completion time, then task quality.
    Faster,
    /// Prefer task-quality score, then predicted completion time.
    HigherQuality,
}

/// Evidence identifier from a trusted catalog, not an assertion by a model.
#[derive(Debug, Clone)]
pub struct QualityEvidence {
    /// SHA-256 of the externally auditable qualification report.
    pub report_sha256: String,
    /// The immutable model digest that report evaluated.
    pub model_sha256: String,
    /// The immutable runtime digest that report evaluated.
    pub runtime_sha256: String,
    /// Exact configuration ID, including context/output/decoding settings.
    pub configuration_id: String,
    /// Frozen task score in basis points, 1..=10000, after release gates pass.
    pub score_basis_points: u16,
}

/// One catalog configuration; no model family or parameter-size ceiling.
///
/// The caller must verify the catalog's provenance and qualification report.
/// This pure selector checks binding and resource fit, not report truthfulness.
#[derive(Debug, Clone)]
pub struct ModelCandidate {
    /// Stable unique configuration ID.
    pub id: String,
    /// Immutable model SHA-256.
    pub model_sha256: String,
    /// Immutable runtime SHA-256.
    pub runtime_sha256: String,
    /// Compatible platform/runtime/language/context configuration.
    pub compatible: bool,
    /// Conservative whole-runtime peak, including KV/output/scratch/children.
    pub peak_private_bytes: u64,
    /// Bytes not yet downloaded, zero for a verified installed configuration.
    pub download_remaining_bytes: u64,
    /// Maximum extra unpack/staging bytes, beyond the remaining download.
    pub unpack_extra_bytes: u64,
    /// Predicted complete-job latency at the selected settings, not a promise.
    pub predicted_completion_ms: u64,
    /// Passing task-quality report; missing evidence is not Recommended.
    pub quality: Option<QualityEvidence>,
}

/// Why a catalog configuration cannot currently be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// Identity, evidence binding, resource estimate or ID is invalid.
    InvalidEvidence,
    /// Duplicate configuration IDs make selection ambiguous.
    DuplicateId,
    /// Platform/runtime/language/context checks refused this configuration.
    Incompatible,
    /// Its complete runtime would exceed the freshly computed budget.
    InsufficientMemory,
    /// Setup would leave less than 2 GiB of free disk, or its sum overflowed.
    InsufficientDisk,
    /// Free disk could not be observed.
    DiskUnknown,
}

/// A declined candidate and actionable structured reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Candidate configuration ID, as supplied by the trusted catalog.
    pub id: String,
    /// First refusal reason; no inference or download has started.
    pub reason: RejectionReason,
}

/// Ordered estimates only: this result never claims Verified on this Mac.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Eligible configuration IDs, first is the recommendation.
    pub eligible: Vec<String>,
    /// Declined entries, including all ambiguous duplicates.
    pub rejected: Vec<Rejection>,
}

/// Filter and rank candidates without side effects or machine-tier branches.
pub fn select_models(
    candidates: &[ModelCandidate],
    budget: MemoryBudget,
    free_disk_bytes: Option<u64>,
    preference: Preference,
) -> Selection {
    let mut counts = HashMap::new();
    for candidate in candidates {
        *counts.entry(candidate.id.as_str()).or_insert(0_usize) += 1;
    }
    let mut eligible = Vec::new();
    let mut rejected = Vec::new();
    for candidate in candidates {
        let reason = if counts[candidate.id.as_str()] > 1 {
            Some(RejectionReason::DuplicateId)
        } else {
            rejection(candidate, budget, free_disk_bytes)
        };
        if let Some(reason) = reason {
            rejected.push(Rejection {
                id: candidate.id.clone(),
                reason,
            });
        } else {
            eligible.push(candidate);
        }
    }
    eligible.sort_by(|a, b| {
        let score = |model: &ModelCandidate| {
            // Only candidates with valid, identity-bound evidence get here.
            u64::from(model.quality.as_ref().unwrap().score_basis_points)
        };
        let primary = match preference {
            Preference::Balanced => balanced_score(b).cmp(&balanced_score(a)),
            Preference::Faster => a.predicted_completion_ms.cmp(&b.predicted_completion_ms),
            Preference::HigherQuality => score(b).cmp(&score(a)),
        };
        primary
            .then_with(|| score(b).cmp(&score(a)))
            .then_with(|| a.predicted_completion_ms.cmp(&b.predicted_completion_ms))
            .then_with(|| a.peak_private_bytes.cmp(&b.peak_private_bytes))
            .then_with(|| a.id.cmp(&b.id))
    });
    Selection {
        eligible: eligible.into_iter().map(|model| model.id.clone()).collect(),
        rejected,
    }
}

fn rejection(
    candidate: &ModelCandidate,
    budget: MemoryBudget,
    free_disk_bytes: Option<u64>,
) -> Option<RejectionReason> {
    let digest = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    let valid = !candidate.id.is_empty()
        && candidate.id.len() <= 128
        && candidate
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
        && candidate.peak_private_bytes > 0
        && candidate.predicted_completion_ms > 0
        && digest(&candidate.model_sha256)
        && digest(&candidate.runtime_sha256)
        && candidate.quality.as_ref().is_some_and(|evidence| {
            digest(&evidence.report_sha256)
                && evidence.model_sha256 == candidate.model_sha256
                && evidence.runtime_sha256 == candidate.runtime_sha256
                && evidence.configuration_id == candidate.id
                && (1..=10_000).contains(&evidence.score_basis_points)
        });
    if !valid {
        return Some(RejectionReason::InvalidEvidence);
    }
    if !candidate.compatible {
        return Some(RejectionReason::Incompatible);
    }
    if candidate.peak_private_bytes > budget.runtime_bytes {
        return Some(RejectionReason::InsufficientMemory);
    }
    let Some(free) = free_disk_bytes else {
        return Some(RejectionReason::DiskUnknown);
    };
    let required = candidate
        .download_remaining_bytes
        .checked_add(candidate.unpack_extra_bytes)
        .and_then(|bytes| bytes.checked_add(2 * GIB));
    if required.is_none_or(|bytes| free < bytes) {
        return Some(RejectionReason::InsufficientDisk);
    }
    None
}

// Fixed normalization avoids changing existing rankings when an unrelated
// candidate is added: speed = floor(10000 * 60000 / (60000 + predicted_ms)).
// Balanced is 75% quality / 25% speed; omitting a common divisor is exact.
fn balanced_score(candidate: &ModelCandidate) -> u64 {
    let speed = 10_000_u128 * 60_000 / (60_000 + u128::from(candidate.predicted_completion_ms));
    u64::from(candidate.quality.as_ref().unwrap().score_basis_points) * 3 + speed as u64
}
