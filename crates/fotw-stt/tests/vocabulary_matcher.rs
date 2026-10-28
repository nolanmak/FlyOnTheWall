//! Fixture table ported from Muesli's matcher behavior (see the fixture's `source`).
use fotw_stt::vocabulary::{Correction, Profile};
use fotw_stt::vocabulary_matcher::VocabularyMatcher;

#[test]
fn correction_fixture_table() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/vocabulary-corrections-v1.json")).unwrap();
    let mut failures = Vec::new();
    let mut total = 0;
    for group in fixture["groups"].as_array().unwrap() {
        let profile = Profile::parse(&serde_json::to_vec(&group["profile"]).unwrap()).unwrap();
        let matcher = VocabularyMatcher::new(&profile);
        for case in group["cases"].as_array().unwrap() {
            total += 1;
            let input = case["input"].as_str().unwrap();
            let expected = case["output"].as_str().unwrap();
            let got = matcher.apply(input);
            if got != expected {
                failures.push(format!(
                    "{} / {}: {input:?} -> {got:?}, expected {expected:?}",
                    group["name"], case["why"]
                ));
            }
        }
    }
    assert!(total >= 25, "fixture table has only {total} cases");
    assert!(
        failures.is_empty(),
        "{} of {total} cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn hints_and_corrections_are_counted_once() {
    let profile = Profile {
        recognition_hints: vec!["Kubernetes".into(), "Deepgram".into()],
        corrections: vec![Correction {
            word: "kubernetes".into(),
            replacement: None,
            matching_threshold: Some(0.9),
        }],
        ..Default::default()
    };
    assert_eq!(VocabularyMatcher::new(&profile).len(), 2);
    assert!(VocabularyMatcher::new(&Profile::default()).is_empty());
}

#[test]
fn old_v1_profiles_load_unchanged_and_corrections_round_trip() {
    let old = br#"{"version":1,"language":"en","recognition_hints":["Kubernetes"],"snippets":[]}"#;
    let profile = Profile::parse(old).unwrap();
    assert!(profile.corrections.is_empty());
    let exported = serde_json::to_string(&profile).unwrap();
    assert!(!exported.contains("corrections"), "{exported}");
    assert_eq!(Profile::parse(exported.as_bytes()).unwrap(), profile);

    let new = br#"{"version":1,"corrections":[{"word":"open telemetry","replacement":"OpenTelemetry","matching_threshold":0.9},{"word":"Caivex"}]}"#;
    let profile = Profile::parse(new).unwrap();
    assert_eq!(profile.corrections[0].matching_threshold, Some(0.9));
    assert_eq!(profile.corrections[1].replacement, None);
    let exported = serde_json::to_vec_pretty(&profile).unwrap();
    assert_eq!(Profile::parse(&exported).unwrap(), profile);
}

#[test]
fn invalid_corrections_are_rejected() {
    for raw in [
        r#"{"version":1,"corrections":[{"word":""}]}"#,
        r#"{"version":1,"corrections":[{"word":" padded"}]}"#,
        r#"{"version":1,"corrections":[{"word":"a\nb"}]}"#,
        r#"{"version":1,"corrections":[{"word":"..."}]}"#,
        r#"{"version":1,"corrections":[{"word":"one two three four five six seven eight nine"}]}"#,
        r#"{"version":1,"corrections":[{"word":"x","replacement":""}]}"#,
        r#"{"version":1,"corrections":[{"word":"x","matching_threshold":0.5}]}"#,
        r#"{"version":1,"corrections":[{"word":"x","matching_threshold":0.99}]}"#,
        r#"{"version":1,"corrections":[{"word":"Open  Telemetry"}]}"#,
        r#"{"version":1,"corrections":[{"word":"open telemetry"},{"word":"OPEN TELEMETRY"}]}"#,
        r#"{"version":1,"corrections":[{"word":"Jose"},{"word":"José","replacement":"José"}]}"#,
        r#"{"version":1,"corrections":[{"word":"Zoe"},{"word":"zoe\u0308"}]}"#,
        r#"{"version":1,"corrections":[{"word":"x","api_key":"fixture"}]}"#,
    ] {
        assert!(Profile::parse(raw.as_bytes()).is_err(), "{raw}");
    }
    let too_many = Profile {
        corrections: (0..=fotw_stt::vocabulary::MAX_CORRECTIONS)
            .map(|i| Correction {
                word: format!("term{i}"),
                replacement: None,
                matching_threshold: None,
            })
            .collect(),
        ..Default::default()
    };
    assert!(too_many.validate().is_err());
}

#[test]
fn accent_only_duplicates_are_rejected_with_a_clear_error() {
    let error =
        Profile::parse(br#"{"version":1,"corrections":[{"word":"Jose"},{"word":"Jos\u00e9"}]}"#)
            .unwrap_err();
    assert!(error.contains("accents"), "{error}");
}
