//! `fotwd serve --port` — the flag that makes the UI's origin stable.
//!
//! # Why this flag has to exist
//!
//! The redeemed bearer lives in `sessionStorage` (ING-08), which is keyed by
//! *origin* — scheme, host and **port**. Binding an ephemeral port therefore
//! mints a new origin on every launch, and the tab's credential is not merely
//! stale but unreachable: a bookmark cannot carry it and a reload cannot find
//! it. Every restart forces a fresh one-time handoff inside its 30-second
//! window, which is exactly the failure a user reports as "the link never
//! works".
//!
//! # Why it is nonetheless opt-in
//!
//! `serve` still defaults to port 0. A fixed port is guessable by a page
//! scanning localhost, and while the port is not itself a security control —
//! ING-01 through ING-05 are — it is one more thing an attacker has to find.
//! Trading that away is the user's call to make explicitly, not ours to make
//! for them.

use fotwd::serve::parse_port;

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn no_flag_leaves_the_choice_to_the_os() {
    assert_eq!(parse_port(&args(&["serve"])), Ok(0));
}

#[test]
fn an_explicit_port_is_taken() {
    assert_eq!(parse_port(&args(&["serve", "--port", "8765"])), Ok(8765));
}

#[test]
fn other_flags_do_not_confuse_it() {
    assert_eq!(
        parse_port(&args(&["serve", "--print-url", "--port", "8765"])),
        Ok(8765)
    );
}

#[test]
fn zero_is_a_legal_way_to_ask_for_ephemeral() {
    assert_eq!(parse_port(&args(&["serve", "--port", "0"])), Ok(0));
}

#[test]
fn a_non_numeric_port_is_refused() {
    let e = parse_port(&args(&["serve", "--port", "http"])).unwrap_err();
    assert!(e.contains("http"), "the bad value should be quoted back: {e}");
}

/// `--port` with nothing after it must not silently mean "ephemeral". Taking
/// the next flag as the value would be worse still: `--port --print-url`
/// would bind whatever `--print-url` parsed to.
#[test]
fn a_missing_value_is_refused() {
    let e = parse_port(&args(&["serve", "--port"])).unwrap_err();
    assert!(!e.is_empty());
    let e = parse_port(&args(&["serve", "--port", "--print-url"])).unwrap_err();
    assert!(!e.is_empty());
}

/// 65536 does not fit a `u16`. Refusing here produces a message naming the
/// flag; letting it overflow into a parse error further down does not.
#[test]
fn an_out_of_range_port_is_refused() {
    assert!(parse_port(&args(&["serve", "--port", "65536"])).is_err());
    assert!(parse_port(&args(&["serve", "--port", "-1"])).is_err());
}

/// Binding below 1024 needs root, and the error the OS returns for that is
/// `Permission denied` with no mention of the port — which sends the reader
/// hunting a TCC or keychain problem that does not exist.
#[test]
fn a_privileged_port_is_refused_with_a_reason() {
    let e = parse_port(&args(&["serve", "--port", "80"])).unwrap_err();
    assert!(
        e.contains("1024"),
        "the message should say where the boundary is: {e}"
    );
}
