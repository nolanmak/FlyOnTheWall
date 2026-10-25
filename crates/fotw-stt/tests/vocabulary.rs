use fotw_stt::vocabulary::{MAX_PROFILE_BYTES, Profile, Snippet};
#[cfg(feature = "conformance")]
#[test]
fn shared_vocabulary_fixture_contract() {
    fotw_stt::conformance::vocabulary();
}
#[test]
fn unsupported_versions_unknown_fields_conflicts_and_limits_are_rejected() {
    for raw in [
        r#"{"version":2}"#,
        r#"{"version":1,"api_key":"fixture"}"#,
        r#"{"version":1,"language":"auto"}"#,
        r#"{"version":1,"recognition_hints":["C++","c++"]}"#,
    ] {
        assert!(Profile::parse(raw.as_bytes()).is_err(), "{raw}");
    }
    let p = Profile {
        snippets: vec![
            Snippet {
                name: "signature".into(),
                text: "one".into(),
            },
            Snippet {
                name: "SIGNATURE".into(),
                text: "two".into(),
            },
        ],
        ..Default::default()
    };
    assert!(p.validate().is_err());
    let p = Profile {
        recognition_hints: vec!["x".repeat(129)],
        ..Default::default()
    };
    assert!(p.validate().is_err());
    let p = Profile {
        recognition_hints: vec!["key\nterm".into()],
        ..Default::default()
    };
    assert!(p.validate().is_err());
    let padded = format!("{}{{\"version\":1}}", " ".repeat(MAX_PROFILE_BYTES));
    assert!(Profile::parse(padded.as_bytes()).is_err());
    assert!(Profile::parse(br#"{"version":1}"#).is_ok());
}
