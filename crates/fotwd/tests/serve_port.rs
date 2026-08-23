//! `fotwd serve --port` — the port is the origin, and the origin is the login.
//!
//! The bearer is stored keyed by *origin* — scheme, host and **port** — so a
//! stable port is what makes `http://127.0.0.1:8737` a bookmark that works
//! and lets every tab share one login. The default is therefore the fixed
//! [`DEFAULT_PORT`]; `--port 0` buys back the old ephemeral behavior, where
//! every restart minted a fresh origin and forced a new 30-second handoff.
//! The port was never a security control — ING-01 through ING-05 are.

use fotwd::serve::{DEFAULT_PORT, parse_port};

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn no_flag_means_the_stable_default_port() {
    assert_eq!(parse_port(&args(&["serve"])), Ok(DEFAULT_PORT));
    assert_ne!(DEFAULT_PORT, 0, "the default origin must survive a restart");
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
    assert!(
        e.contains("http"),
        "the bad value should be quoted back: {e}"
    );
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

// ------------------------------------------------------- the double-click

use fotwd::serve::{BareLaunch, bare_launch};

/// Finder launches `CFBundleExecutable` with no arguments and no terminal.
/// Printing usage to a stdout nobody can see and exiting is indistinguishable
/// from "the app doesn't open" — which is precisely how it was reported. A
/// bare launch with no terminal is the doorway: serve.
#[test]
fn a_finder_launch_is_the_doorway() {
    assert_eq!(bare_launch(false), BareLaunch::Serve);
}

/// A person typing `fotwd` in a terminal is asking what it does. Usage is
/// the correct answer there, exactly as before.
#[test]
fn a_bare_terminal_invocation_still_prints_usage() {
    assert_eq!(bare_launch(true), BareLaunch::Usage);
}
