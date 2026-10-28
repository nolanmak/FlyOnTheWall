use fotw_summarize::local_model::*;

fn host(gib: u64) -> HardwareSnapshot {
    HardwareSnapshot {
        supported_platform: true,
        physical_bytes: Some(gib * GIB),
        available_bytes: Some(gib * GIB),
        owned_resident_bytes: 0,
        pressure: Some(MemoryPressure::Normal),
    }
}

fn candidate(id: &str, footprint: u64, score: u16, latency: u64) -> ModelCandidate {
    ModelCandidate {
        id: id.into(),
        model_sha256: "a".repeat(64),
        runtime_sha256: "b".repeat(64),
        compatible: true,
        peak_private_bytes: footprint,
        download_remaining_bytes: 0,
        unpack_extra_bytes: 0,
        predicted_completion_ms: latency,
        quality: Some(QualityEvidence {
            report_sha256: "c".repeat(64),
            model_sha256: "a".repeat(64),
            runtime_sha256: "b".repeat(64),
            configuration_id: id.into(),
            score_basis_points: score,
        }),
    }
}

#[test]
fn scales_continuously_through_future_mac_capacities() {
    for gib in [
        8, 16, 24, 32, 36, 48, 64, 96, 128, 192, 256, 512, 1024, 1536,
    ] {
        let budget = memory_budget(host(gib), None).expect("a healthy Mac must have a budget");
        assert_eq!(
            budget.runtime_bytes,
            (u128::from(gib * GIB) * 7 / 10) as u64
        );
    }
    let models = [
        candidate("small", GIB, 9500, 60_000),
        candidate("large", 160 * GIB, 9900, 900_000),
    ];
    let small = select_models(
        &models,
        memory_budget(host(24), None).unwrap(),
        Some(2000 * GIB),
        Preference::HigherQuality,
    );
    let large = select_models(
        &models,
        memory_budget(host(512), None).unwrap(),
        Some(2000 * GIB),
        Preference::HigherQuality,
    );
    assert_eq!(small.eligible, ["small"]);
    assert_eq!(
        large.eligible,
        ["large", "small"],
        "512 GiB must not be capped to the 24 GiB profile"
    );
}

#[test]
fn current_headroom_pressure_and_user_cap_override_installed_ram() {
    let mut snapshot = host(512);
    snapshot.available_bytes = Some(60 * GIB);
    let budget = memory_budget(snapshot, Some(4 * GIB)).unwrap();
    assert_eq!(budget.runtime_bytes, 4 * GIB);
    snapshot.pressure = Some(MemoryPressure::Warning);
    assert_eq!(
        memory_budget(snapshot, None),
        Err(AdmissionError::MemoryPressure)
    );
    snapshot.pressure = None;
    assert_eq!(
        memory_budget(snapshot, None),
        Err(AdmissionError::MemoryUnknown)
    );
}

#[test]
fn choices_respect_quality_speed_and_stable_ties() {
    let mut models = vec![
        candidate("large", 4 * GIB, 9900, 900_000),
        candidate("small", GIB, 9500, 60_000),
    ];
    let budget = memory_budget(host(24), None).unwrap();
    for (preference, expected) in [
        (Preference::HigherQuality, "large"),
        (Preference::Faster, "small"),
        (Preference::Balanced, "small"),
    ] {
        assert_eq!(
            select_models(&models, budget, Some(100 * GIB), preference).eligible[0],
            expected
        );
    }
    models.push(candidate("a-small", GIB, 9500, 60_000));
    models.reverse();
    assert_eq!(
        select_models(&models, budget, Some(100 * GIB), Preference::Faster).eligible,
        ["a-small", "small", "large"]
    );
}

#[test]
fn unqualified_or_wrong_model_runtime_context_cannot_be_recommended() {
    let mut models = Vec::new();
    for index in 0..6 {
        let mut model = candidate(&format!("invalid-{index}"), GIB, 9500, 1000);
        match index {
            0 => model.quality = None,
            1 => model.quality.as_mut().unwrap().model_sha256 = "d".repeat(64),
            2 => model.quality.as_mut().unwrap().runtime_sha256 = "d".repeat(64),
            3 => model.quality.as_mut().unwrap().configuration_id = "another-context".into(),
            4 => model.quality.as_mut().unwrap().report_sha256.clear(),
            _ => model.quality.as_mut().unwrap().score_basis_points = 10001,
        }
        models.push(model);
    }
    let result = select_models(
        &models,
        MemoryBudget {
            reserve_bytes: 2 * GIB,
            runtime_bytes: 10 * GIB,
        },
        Some(100 * GIB),
        Preference::Balanced,
    );
    assert!(result.eligible.is_empty());
    assert_eq!(result.rejected.len(), models.len());
    assert!(
        result
            .rejected
            .iter()
            .all(|r| r.reason == RejectionReason::InvalidEvidence)
    );
}

#[test]
fn unknown_inconsistent_and_unsupported_snapshots_fail_closed() {
    let mut snapshots = Vec::new();
    let mut missing = host(8);
    missing.physical_bytes = None;
    snapshots.push(missing);
    let mut missing = host(8);
    missing.available_bytes = None;
    snapshots.push(missing);
    let mut invalid = host(8);
    invalid.available_bytes = Some(9 * GIB);
    snapshots.push(invalid);
    let mut invalid = host(8);
    invalid.owned_resident_bytes = 1;
    snapshots.push(invalid);
    for snapshot in snapshots {
        assert_eq!(
            memory_budget(snapshot, None),
            Err(AdmissionError::MemoryUnknown)
        );
    }
    let mut unsupported = host(8);
    unsupported.supported_platform = false;
    assert_eq!(
        memory_budget(unsupported, None),
        Err(AdmissionError::UnsupportedPlatform)
    );
    let mut critical = host(512);
    critical.pressure = Some(MemoryPressure::Critical);
    assert_eq!(
        memory_budget(critical, None),
        Err(AdmissionError::MemoryPressure)
    );
}

#[test]
fn resident_private_memory_is_not_charged_twice_and_pressure_is_reobserved() {
    let mut snapshot = host(24);
    snapshot.available_bytes = Some(8 * GIB);
    snapshot.owned_resident_bytes = 4 * GIB;
    let budget = memory_budget(snapshot, None).unwrap();
    assert_eq!(budget.runtime_bytes, 12 * GIB - budget.reserve_bytes);
    snapshot.available_bytes = Some(GIB);
    snapshot.owned_resident_bytes = 0;
    assert_eq!(memory_budget(snapshot, None).unwrap().runtime_bytes, 0);
    assert_eq!(memory_budget(host(24), Some(0)).unwrap().runtime_bytes, 0);
}

#[test]
fn integer_rounding_and_u64_limits_do_not_inflate_the_budget() {
    let snapshot = HardwareSnapshot {
        physical_bytes: Some(u64::MAX),
        available_bytes: Some(u64::MAX),
        ..host(8)
    };
    let budget = memory_budget(snapshot, None).unwrap();
    assert_eq!(budget.reserve_bytes, u64::MAX.div_ceil(10));
    assert_eq!(budget.runtime_bytes, (u128::from(u64::MAX) * 7 / 10) as u64);
    let mut overflow = snapshot;
    overflow.owned_resident_bytes = 1;
    assert_eq!(
        memory_budget(overflow, None),
        Err(AdmissionError::MemoryUnknown)
    );
}

#[test]
fn exact_disk_and_memory_bounds_are_inclusive_and_overflow_is_refused() {
    let mut model = candidate("fixture", 3 * GIB, 9500, 1000);
    model.download_remaining_bytes = GIB;
    model.unpack_extra_bytes = 2 * GIB;
    let budget = MemoryBudget {
        reserve_bytes: 2 * GIB,
        runtime_bytes: 3 * GIB,
    };
    let select =
        |model: ModelCandidate, disk| select_models(&[model], budget, disk, Preference::Balanced);
    assert_eq!(select(model.clone(), Some(5 * GIB)).eligible, ["fixture"]);
    assert_eq!(
        select(model.clone(), Some(5 * GIB - 1)).rejected[0].reason,
        RejectionReason::InsufficientDisk
    );
    assert_eq!(
        select(model.clone(), None).rejected[0].reason,
        RejectionReason::DiskUnknown
    );
    model.peak_private_bytes += 1;
    assert_eq!(
        select(model.clone(), Some(5 * GIB)).rejected[0].reason,
        RejectionReason::InsufficientMemory
    );
    model.peak_private_bytes -= 1;
    model.download_remaining_bytes = u64::MAX;
    assert_eq!(
        select(model, Some(u64::MAX)).rejected[0].reason,
        RejectionReason::InsufficientDisk
    );
}

#[test]
fn ambiguous_duplicate_ids_and_incompatible_models_cannot_win() {
    let duplicate = candidate("same", GIB, 9900, 1000);
    let mut incompatible = candidate("unsupported", GIB, 9900, 1000);
    incompatible.compatible = false;
    let result = select_models(
        &[duplicate.clone(), duplicate, incompatible],
        MemoryBudget {
            reserve_bytes: 2 * GIB,
            runtime_bytes: 8 * GIB,
        },
        Some(100 * GIB),
        Preference::HigherQuality,
    );
    assert!(result.eligible.is_empty());
    assert_eq!(
        result.rejected.iter().map(|r| r.reason).collect::<Vec<_>>(),
        [
            RejectionReason::DuplicateId,
            RejectionReason::DuplicateId,
            RejectionReason::Incompatible
        ]
    );
}
