//! Audio retention, the disk budget, and the sweeper (docs/REQUIREMENTS.md
//! §9.5).
//!
//! The sweeper is a pure function of `(now, meetings, settings)`, which is the
//! entire reason these read as arithmetic rather than as filesystem fixtures
//! with a mocked clock. Every rule in §9.5 gets a test that fails if the rule
//! is removed, and — more importantly — the *protections* get tests too: a
//! sweeper that deleted everything would satisfy "reclaims space" perfectly.

use std::path::PathBuf;

use fotw_pipeline::retention::{
    DEFAULT_BUDGET_BYTES, DEFAULT_RETAIN_DAYS, HEAVY_USER, MS_PER_DAY, MeetingAudio,
    RetentionPolicy, RetentionSettings, SweepReason, SweepWarning, TranscriptState, dir_bytes,
    plan_sweep, usage,
};

const GIB: u64 = 1024 * 1024 * 1024;
/// An arbitrary "now" well clear of the epoch, so `saturating_sub` in the
/// implementation cannot mask an arithmetic error by clamping at zero.
const NOW: u64 = 1_800_000_000_000;

fn meeting(id: &str, started_days_ago: u64) -> MeetingAudio {
    let started = NOW - started_days_ago * MS_PER_DAY;
    MeetingAudio {
        state: TranscriptState::Ready,
        transcript_ready_at_ms: Some(started + 60_000),
        audio_bytes: 16 * 1024 * 1024,
        transcript_bytes: 40 * 1024,
        audio_paths: vec![
            PathBuf::from(format!("2026/08/{id}/system.opus")),
            PathBuf::from(format!("2026/08/{id}/mic.opus")),
        ],
        ..MeetingAudio::new(id, started)
    }
}

fn evicted(plan: &fotw_pipeline::retention::SweepPlan) -> Vec<&str> {
    plan.evictions
        .iter()
        .map(|e| e.meeting_id.as_str())
        .collect()
}

#[test]
fn the_default_deletes_thirty_days_after_the_transcript_is_ready_and_not_before() {
    let s = RetentionSettings::default();
    assert_eq!(s.default_days, DEFAULT_RETAIN_DAYS);

    // Day 29 and 31 relative to *ready*, not to the meeting. The `+60_000` in
    // the fixture is what makes those two clocks distinguishable.
    let young = meeting("young", 29);
    let old = meeting("old", 31);
    let plan = plan_sweep(NOW, &[young, old], &s);

    assert_eq!(evicted(&plan), ["old"], "exactly the expired meeting goes");
    assert_eq!(
        plan.evictions[0].reason,
        SweepReason::PolicyAge { days: 30 }
    );
}

#[test]
fn the_clock_starts_at_ready_not_at_the_meeting() {
    // A meeting recorded 100 days ago whose transcript only landed yesterday
    // must keep its audio: the window exists so the user can check the
    // transcript against the recording, and that window opens when the
    // transcript does.
    let mut m = meeting("slow", 100);
    m.transcript_ready_at_ms = Some(NOW - MS_PER_DAY);
    let plan = plan_sweep(NOW, &[m], &RetentionSettings::default());
    assert!(
        plan.evictions.is_empty(),
        "audio was deleted 29 days before the window it was promised"
    );
}

#[test]
fn audio_with_no_transcript_is_never_deleted_by_an_age_rule() {
    // Whatever the age, un-transcribed audio is the only copy of the meeting.
    // `failed` is the case that matters most: transcription broke, and the
    // recording is all the user has left.
    let mut recording = meeting("recording", 400);
    recording.state = TranscriptState::Recording;
    recording.transcript_ready_at_ms = None;

    let mut transcribing = meeting("transcribing", 400);
    transcribing.state = TranscriptState::Transcribing;
    transcribing.transcript_ready_at_ms = None;

    let mut failed = meeting("failed", 400);
    failed.state = TranscriptState::Failed;
    failed.transcript_ready_at_ms = None;

    let plan = plan_sweep(
        NOW,
        &[recording, transcribing, failed],
        &RetentionSettings::default(),
    );
    assert!(
        plan.evictions.is_empty(),
        "un-transcribed audio was swept: {:?}",
        evicted(&plan)
    );
}

/// The single irreversible mistake the sweeper can make, and the one §9.5 as
/// written would have made.
///
/// §9.5 exempts only `forever` from budget eviction. It says nothing about a
/// meeting whose transcript does not exist, and there is a shape — produced by
/// the shipping code, not by a contrived fixture — where that omission
/// destroys the only copy of a meeting: `fotwd` marks a session `ready` when
/// it finishes **whether or not a provider was configured**, because recording
/// without transcription is a supported mode (see `persist_session`). So
/// `state = ready` does not mean "there is a transcript".
///
/// Age rules were already safe, because they key off `transcript_ready_at_ms`
/// and that is `None` here. Budget eviction was not: it keyed off `state`
/// alone, so the first time a user with transcription switched off crossed
/// their 20 GiB budget, the sweeper would have deleted meetings that had never
/// been transcribed — audio that is not a backup of anything, with no text
/// left behind to say what was lost.
#[test]
fn eviction_never_deletes_audio_for_a_meeting_with_no_transcript() {
    let settings = RetentionSettings {
        default_days: 30,
        budget_bytes: GIB,
    };

    // Recorded with no provider configured: finished, marked ready, no
    // transcript row, no segments, and ten times over budget.
    let mut untranscribed = meeting("no-transcript", 90);
    untranscribed.state = TranscriptState::Ready;
    untranscribed.transcript_ready_at_ms = None;
    untranscribed.transcript_bytes = 0;
    untranscribed.audio_bytes = 10 * GIB;

    let plan = plan_sweep(NOW, &[untranscribed.clone()], &settings);

    assert!(
        plan.evictions.is_empty(),
        "the sweeper deleted the only copy of an untranscribed meeting: {:?}",
        evicted(&plan)
    );
    assert!(!plan.within_budget(), "it should still report being over");
    assert!(
        plan.warnings
            .contains(&SweepWarning::OnlyUntranscribedRemains {
                over_by_bytes: 9 * GIB,
                untranscribed_bytes: 10 * GIB,
            }),
        "over budget and unable to act, it must say so rather than pass \
         silently; warnings were {:?}",
        plan.warnings
    );

    // And it is not simply that nothing was evictable: a sibling that *does*
    // have a transcript goes, so the protection is specific rather than the
    // sweeper being inert.
    let transcribed = meeting("has-transcript", 91);
    let plan = plan_sweep(NOW, &[untranscribed, transcribed], &settings);
    assert_eq!(evicted(&plan), ["has-transcript"]);
}

/// The accounting has to agree with the protection, or the settings screen
/// tells the user 10 GB is reclaimable when none of it is.
#[test]
fn a_ready_meeting_with_no_transcript_counts_as_untranscribed_in_the_usage_split() {
    let mut m = meeting("ready-but-empty", 1);
    m.state = TranscriptState::Ready;
    m.transcript_ready_at_ms = None;
    m.audio_bytes = 4 * GIB;

    let u = usage(&[m]);
    assert_eq!(u.untranscribed_bytes, 4 * GIB);
}

#[test]
fn the_four_per_meeting_overrides_each_do_what_they_say() {
    let s = RetentionSettings::default();

    let mut forever = meeting("forever", 3_650);
    forever.policy = RetentionPolicy::Forever;

    let mut until = meeting("until", 1);
    until.policy = RetentionPolicy::UntilTranscribed;

    let mut week = meeting("week", 8);
    week.policy = RetentionPolicy::Days { days: 7 };

    let mut year = meeting("year", 200);
    year.policy = RetentionPolicy::Days { days: 365 };

    let default_old = meeting("default-old", 31);

    let plan = plan_sweep(NOW, &[forever, until, week, year, default_old], &s);
    let mut got = evicted(&plan);
    got.sort_unstable();

    assert_eq!(
        got,
        ["default-old", "until", "week"],
        "forever (10 years old) and days(365) (200 days old) must both survive"
    );

    let reason = |id: &str| {
        plan.evictions
            .iter()
            .find(|e| e.meeting_id == id)
            .unwrap()
            .reason
    };
    // `until_transcribed` deletes the moment the transcript exists — the
    // meeting above is a day old and inside every other policy's window.
    assert_eq!(reason("until"), SweepReason::Transcribed);
    assert_eq!(reason("week"), SweepReason::PolicyAge { days: 7 });
    assert_eq!(reason("default-old"), SweepReason::PolicyAge { days: 30 });
}

#[test]
fn a_forever_meeting_is_never_touched_by_anything() {
    // The escape hatch has to be absolute, or it is not an escape hatch. Age,
    // budget, and both together.
    let mut keep = meeting("keep", 5_000);
    keep.policy = RetentionPolicy::Forever;
    keep.audio_bytes = 30 * GIB;

    let plan = plan_sweep(
        NOW,
        &[keep],
        &RetentionSettings {
            default_days: 30,
            budget_bytes: GIB,
        },
    );
    assert!(
        plan.evictions.is_empty(),
        "a `forever` meeting was evicted for budget"
    );
    assert!(!plan.within_budget());
    assert!(
        matches!(
            plan.warnings.as_slice(),
            [SweepWarning::OnlyForeverRemains { over_by_bytes, forever_bytes }]
                if *over_by_bytes == 29 * GIB && *forever_bytes == 30 * GIB
        ),
        "expected a warning naming the forever bytes, got {:?}",
        plan.warnings
    );
}

#[test]
fn the_budget_evicts_oldest_first_and_stops_the_moment_it_is_under() {
    // Five 5 GiB meetings, all inside their retention window, against a 12 GiB
    // budget: 25 GiB down to <= 12 needs the three oldest and no more.
    let settings = RetentionSettings {
        default_days: 30,
        budget_bytes: 12 * GIB,
    };
    let meetings: Vec<MeetingAudio> = (0..5)
        .map(|i| {
            let mut m = meeting(&format!("m{i}"), 5 - i);
            m.audio_bytes = 5 * GIB;
            m
        })
        .collect();

    let plan = plan_sweep(NOW, &meetings, &settings);
    assert_eq!(
        evicted(&plan),
        ["m0", "m1", "m2"],
        "eviction must run oldest-first and stop as soon as it is under budget"
    );
    assert!(
        plan.evictions
            .iter()
            .all(|e| e.reason == SweepReason::OverBudget)
    );
    assert_eq!(plan.audio_bytes_before, 25 * GIB);
    assert_eq!(plan.audio_bytes_after, 10 * GIB);
    assert!(plan.within_budget());
    assert!(
        plan.warnings.is_empty(),
        "a sweep that got under budget has nothing to warn about"
    );
}

#[test]
fn the_budget_warns_rather_than_evicting_when_only_protected_meetings_remain() {
    // §9.5's exact wording: it "warns rather than silently evicting when only
    // `forever` remains". The un-transcribed case is this project's addition,
    // for the same reason.
    let settings = RetentionSettings {
        default_days: 30,
        budget_bytes: 2 * GIB,
    };

    let mut keep = meeting("keep", 10);
    keep.policy = RetentionPolicy::Forever;
    keep.audio_bytes = 6 * GIB;

    let mut pending = meeting("pending", 1);
    pending.state = TranscriptState::Transcribing;
    pending.transcript_ready_at_ms = None;
    pending.audio_bytes = 3 * GIB;

    let mut ordinary = meeting("ordinary", 5);
    ordinary.audio_bytes = GIB;

    let plan = plan_sweep(NOW, &[keep, pending, ordinary], &settings);

    assert_eq!(
        evicted(&plan),
        ["ordinary"],
        "only the unprotected meeting may be evicted"
    );
    assert_eq!(plan.audio_bytes_after, 9 * GIB);
    assert!(!plan.within_budget());

    let over = 7 * GIB;
    assert!(
        plan.warnings.contains(&SweepWarning::OnlyForeverRemains {
            over_by_bytes: over,
            forever_bytes: 6 * GIB,
        }),
        "warnings were {:?}",
        plan.warnings
    );
    assert!(
        plan.warnings
            .contains(&SweepWarning::OnlyUntranscribedRemains {
                over_by_bytes: over,
                untranscribed_bytes: 3 * GIB,
            }),
        "warnings were {:?}",
        plan.warnings
    );
}

#[test]
fn transcripts_are_never_subject_to_retention() {
    // §9.5 is unconditional about this, and it is the one rule where a bug
    // costs the user something irreplaceable. Two assertions, because they
    // fail differently: the accounting must be unchanged, and no path in the
    // plan may even *name* a transcript file.
    let settings = RetentionSettings {
        default_days: 30,
        budget_bytes: 0, // maximum pressure: evict everything you are allowed to
    };
    let meetings: Vec<MeetingAudio> = (0..4).map(|i| meeting(&format!("m{i}"), 90 + i)).collect();
    let text_before: u64 = meetings.iter().map(|m| m.transcript_bytes).sum();
    assert!(text_before > 0);

    let plan = plan_sweep(NOW, &meetings, &settings);
    assert_eq!(plan.evictions.len(), 4, "all four should lose their audio");
    assert_eq!(plan.audio_bytes_after, 0);

    for ev in &plan.evictions {
        for p in &ev.paths {
            let name = p.to_string_lossy();
            assert!(
                name.ends_with(".opus"),
                "the sweeper scheduled a non-audio file for deletion: {name}"
            );
        }
    }

    // The accounting proves the text is untouched: nothing the plan reclaims
    // comes out of `transcript_bytes`.
    let after = usage(&meetings);
    assert_eq!(after.transcript_bytes, text_before);
    assert_eq!(plan.bytes_reclaimed(), plan.audio_bytes_before);
}

#[test]
fn the_plan_deletes_exactly_the_files_it_named_and_nothing_else() {
    let root = std::env::temp_dir().join(format!("fotw-retention-apply-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("2026/08/doomed");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("system.opus"), vec![7u8; 4_096]).unwrap();
    std::fs::write(dir.join("mic.opus"), vec![7u8; 2_048]).unwrap();
    // The transcript artifact §9.6 calls "the most commonly forgotten one".
    std::fs::write(dir.join("raw-deepgram.json.zst"), vec![1u8; 512]).unwrap();

    let mut m = meeting("doomed", 90);
    m.audio_bytes = dir_bytes(&dir).unwrap();
    assert_eq!(m.audio_bytes, 4_096 + 2_048 + 512);
    // Only the two Opus files are audio; the sweeper is told so explicitly.
    m.audio_bytes = 4_096 + 2_048;
    m.audio_paths = vec![
        PathBuf::from("2026/08/doomed/system.opus"),
        PathBuf::from("2026/08/doomed/mic.opus"),
    ];

    let plan = plan_sweep(NOW, &[m], &RetentionSettings::default());
    let (reclaimed, errors) = plan.apply(&root);

    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(reclaimed, 6_144);
    assert!(!dir.join("system.opus").exists());
    assert!(!dir.join("mic.opus").exists());
    assert!(
        dir.join("raw-deepgram.json.zst").exists(),
        "the sweeper deleted a transcript artifact"
    );

    // Idempotent: a second apply finds nothing and reports no error, because
    // the plan is recomputed from the filesystem every time.
    let (again, errors) = plan.apply(&root);
    assert_eq!(again, 0);
    assert!(errors.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_thirty_day_default_produces_the_1_7_gb_steady_state_that_justifies_it() {
    // §9.5 justifies the default with a number. If the arithmetic behind that
    // number is wrong, the default is not defensible and the whole policy
    // should be reconsidered — so the number is asserted, not narrated.
    let per_year = HEAVY_USER.bytes_per_year();
    let gb = per_year as f64 / 1e9;
    assert!(
        (20.0..20.6).contains(&gb),
        "§9.5 says a heavy user generates 20.3 GB/yr; this profile says {gb:.1}"
    );

    let steady = HEAVY_USER.steady_state_bytes(DEFAULT_RETAIN_DAYS);
    let steady_gb = steady as f64 / 1e9;
    assert!(
        (1.6..1.8).contains(&steady_gb),
        "§9.5's defensible steady state is ~1.7 GB; got {steady_gb:.2} GB"
    );

    // And it must sit comfortably inside the default budget, or the default
    // policy and the default budget contradict each other.
    assert!(
        steady < DEFAULT_BUDGET_BYTES / 4,
        "the default retention window nearly fills the default budget on its own"
    );
}

#[test]
fn disk_accounting_splits_audio_from_text_and_names_the_protected_bytes() {
    let mut keep = meeting("keep", 1);
    keep.policy = RetentionPolicy::Forever;
    keep.audio_bytes = 3 * GIB;

    let mut pending = meeting("pending", 0);
    pending.state = TranscriptState::Recording;
    pending.transcript_ready_at_ms = None;
    pending.audio_bytes = GIB;
    pending.transcript_bytes = 0;

    let mut ordinary = meeting("ordinary", 2);
    ordinary.audio_bytes = 2 * GIB;
    ordinary.transcript_bytes = 100;

    let u = usage(&[keep.clone(), pending, ordinary]);
    assert_eq!(u.meetings, 3);
    assert_eq!(u.audio_bytes, 6 * GIB);
    assert_eq!(u.transcript_bytes, keep.transcript_bytes + 100);
    assert_eq!(u.forever_bytes, 3 * GIB);
    assert_eq!(u.untranscribed_bytes, GIB);
    assert_eq!(u.total_bytes(), u.audio_bytes + u.transcript_bytes);
    assert!((u.budget_fraction(12 * GIB) - 0.5).abs() < 1e-9);
}

#[test]
fn the_policy_round_trips_through_the_two_columns_the_schema_uses() {
    // §9.3 splits the policy across `retain_audio` and `retain_audio_days`.
    for p in [
        RetentionPolicy::Default,
        RetentionPolicy::Forever,
        RetentionPolicy::UntilTranscribed,
        RetentionPolicy::Days { days: 90 },
    ] {
        let (kind, days) = p.to_columns();
        assert_eq!(RetentionPolicy::from_columns(kind, days), p);
    }

    // A row from a future version, or a corrupt one, must fall back to the
    // conservative reading rather than to "delete now".
    assert_eq!(
        RetentionPolicy::from_columns("something_new", None),
        RetentionPolicy::Default
    );
    assert_eq!(
        RetentionPolicy::from_columns("days", None),
        RetentionPolicy::Default
    );
    assert_eq!(
        TranscriptState::from_column("nonsense"),
        TranscriptState::Recording
    );
}

#[test]
fn the_sweeper_is_a_pure_function_of_its_arguments() {
    // The property the whole design rests on: same inputs, same plan, no
    // hidden clock and no filesystem. If this ever fails, none of the tests
    // above mean anything, because they would be asserting against ambient
    // state.
    let meetings: Vec<MeetingAudio> = (0..6)
        .map(|i| {
            let mut m = meeting(&format!("m{i}"), i * 10);
            m.audio_bytes = GIB;
            if i == 3 {
                m.policy = RetentionPolicy::Forever;
            }
            m
        })
        .collect();
    let settings = RetentionSettings {
        default_days: 30,
        budget_bytes: 2 * GIB,
    };

    let a = plan_sweep(NOW, &meetings, &settings);
    let b = plan_sweep(NOW, &meetings, &settings);
    assert_eq!(a, b);

    // And it moves with `now` rather than with the wall clock.
    let later = plan_sweep(NOW + 3_650 * MS_PER_DAY, &meetings, &settings);
    assert!(later.evictions.len() > a.evictions.len());
    assert!(
        !later.evictions.iter().any(|e| e.meeting_id == "m3"),
        "ten years later, `forever` still means forever"
    );
}
