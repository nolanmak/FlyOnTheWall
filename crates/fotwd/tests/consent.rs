//! Consent as a product feature, not a paragraph in a ToS.
//!
//! This is the one axis where the project is deliberately different, and the
//! reason is litigation, not taste. *Chamberlain v. Granola* (N.D. Cal.,
//! 2026-07-30) pleads CIPA §§631/632 at **$5,000 per violation** — a
//! per-recorded-participant multiplier — and quotes the defendant's own
//! marketing that participants "won't know it's there". The consolidated
//! Otter.ai litigation attacks *outsourcing* consent to customers via ToS
//! instead of building it into the product.
//!
//! So these are behavioural tests, not documentation.

use fotwd::consent::{ConsentRegime, DisclosureKit, Escalation, JurisdictionSignals, Rules};

fn rules() -> Rules {
    Rules::builtin()
}

#[test]
fn the_rules_table_covers_every_us_state_and_the_named_countries() {
    let r = rules();
    assert_eq!(
        r.us_states().count(),
        50,
        "a missing state silently downgrades that user to the one-party path"
    );
    for code in [
        "DE", "FR", "GB", "IE", "NL", "ES", "IT", "CA", "AU", "NZ", "JP", "SG", "IN", "BR",
    ] {
        assert!(r.get(code).is_some(), "{code} missing from the rules table");
    }
}

#[test]
fn every_rule_carries_a_statute_and_a_citation() {
    // A warning the user may rely on has to say what it is based on. An
    // uncited claim about criminal liability is worse than no claim.
    for j in rules().all() {
        assert!(!j.statute.is_empty(), "{} has no statute", j.code);
        assert!(
            j.citation_url.starts_with("http"),
            "{} has no citation URL",
            j.code
        );
    }
}

#[test]
fn the_known_all_party_states_are_all_party() {
    let r = rules();
    for code in [
        "US-CA", "US-FL", "US-IL", "US-MD", "US-MA", "US-MT", "US-PA", "US-WA",
    ] {
        assert!(
            matches!(
                r.get(code).unwrap().regime,
                ConsentRegime::AllParty | ConsentRegime::Contested
            ),
            "{code} must not be treated as one-party"
        );
    }
}

/// Sources genuinely disagree on these. A confidently wrong warning is worse
/// than a general one, and could itself be relied upon — so the contested
/// ones are marked and escalated rather than guessed.
#[test]
fn contested_jurisdictions_are_marked_and_escalate() {
    let r = rules();
    for code in ["US-NV", "US-CT", "US-OR", "US-MI", "US-HI"] {
        let j = r.get(code).unwrap();
        assert_eq!(
            j.regime,
            ConsentRegime::Contested,
            "{code} is disputed between sources and must be marked contested"
        );
        assert!(!j.note.is_empty(), "{code} must explain what is disputed");
    }
}

#[test]
fn a_one_party_home_with_no_other_signal_gets_a_reminder_not_a_modal() {
    let e = rules().escalate(&JurisdictionSignals::home("US-NY"));
    assert!(matches!(e, Escalation::Reminder { .. }));
}

#[test]
fn an_all_party_home_blocks_and_names_the_statute() {
    let e = rules().escalate(&JurisdictionSignals::home("US-CA"));
    match e {
        Escalation::Blocking { jurisdictions, .. } => {
            assert!(jurisdictions.iter().any(|j| j.code == "US-CA"));
            assert!(
                jurisdictions.iter().any(|j| j.statute.contains("632")),
                "the modal must name the statute, not just say 'all-party'"
            );
        }
        other => panic!("expected a blocking modal, got {other:?}"),
    }
}

/// The signal that actually catches people out: a one-party user on a call
/// with an EU colleague. Germany §201 StGB is *criminal*, up to three years.
#[test]
fn an_attendee_in_a_criminal_jurisdiction_escalates_a_one_party_user() {
    let signals = JurisdictionSignals::home("US-NY")
        .with_attendee_domains(["colleague@example.de", "me@example.com"]);
    match rules().escalate(&signals) {
        Escalation::Blocking { jurisdictions, .. } => {
            let de = jurisdictions.iter().find(|j| j.code == "DE").expect("DE");
            assert!(de.criminal, "§201 StGB is criminal, not merely civil");
            assert!(de.statute.contains("201"));
        }
        other => panic!("a .de attendee must escalate, got {other:?}"),
    }
}

#[test]
fn an_event_timezone_is_a_signal_too() {
    let signals = JurisdictionSignals::home("US-NY").with_event_timezone("Europe/Berlin");
    assert!(matches!(
        rules().escalate(&signals),
        Escalation::Blocking { .. }
    ));
}

#[test]
fn a_cctld_that_is_not_a_country_does_not_escalate() {
    // .com/.io/.ai are not geography. Treating them as such would fire the
    // blocking modal on nearly every meeting, and a modal that always fires
    // is a modal nobody reads.
    let signals = JurisdictionSignals::home("US-NY").with_attendee_domains([
        "a@example.com",
        "b@thing.io",
        "c@lab.ai",
    ]);
    assert!(matches!(
        rules().escalate(&signals),
        Escalation::Reminder { .. }
    ));
}

#[test]
fn escalation_is_never_silent_about_not_being_legal_advice() {
    for signals in [
        JurisdictionSignals::home("US-NY"),
        JurisdictionSignals::home("US-CA"),
    ] {
        let text = rules().escalate(&signals).user_text();
        assert!(
            text.to_lowercase().contains("not legal advice"),
            "every consent surface must disclaim, got: {text}"
        );
    }
}

#[test]
fn an_unknown_home_jurisdiction_escalates_rather_than_assuming_the_permissive_case() {
    // Failing open here means a user in an unlisted country gets the
    // one-party path by default, which is the exact wrong direction to be
    // wrong in.
    let e = rules().escalate(&JurisdictionSignals::home("XX-ZZ"));
    assert!(
        matches!(e, Escalation::Blocking { .. }),
        "an unknown jurisdiction must bias toward over-warning"
    );
}

// ------------------------------------------------------------ disclosure kit

#[test]
fn the_disclosure_notice_is_short_editable_and_offers_an_out() {
    let kit = DisclosureKit::default();
    let notice = kit.chat_notice();
    assert!(
        notice.len() < 200,
        "a notice nobody reads is not disclosure"
    );
    assert!(
        notice.to_lowercase().contains("let me know") || notice.to_lowercase().contains("prefer"),
        "the notice must offer participants a way to object: {notice}"
    );
}

#[test]
fn the_kit_provides_every_channel_the_spec_names() {
    let kit = DisclosureKit::default();
    assert!(!kit.chat_notice().is_empty());
    assert!(!kit.calendar_blurb().is_empty());
    assert!(!kit.verbal_script().is_empty());

    let mailto = kit.consent_mailto(&["a@example.com", "b@example.com"], "Weekly sync");
    assert!(mailto.starts_with("mailto:"));
    assert!(mailto.contains("a@example.com"));
    // Nothing is sent from the app and there is no server: it opens the
    // user's own mail client, which is what keeps this backend-free.
    assert!(mailto.contains("subject="));
    assert!(mailto.contains("body="));
}

#[test]
fn mailto_percent_encodes_rather_than_breaking_on_punctuation() {
    let kit = DisclosureKit::default();
    let m = kit.consent_mailto(&["x@example.com"], "Q3 planning & budget review");
    assert!(!m.contains("& budget"), "unencoded & truncates the mailto");
    assert!(m.contains("%26") || m.contains("%20budget"));
}
