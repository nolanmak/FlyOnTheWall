//! Engine names a test may safely use — #83.
//!
//! A test that configures an engine is one basename away from configuring the
//! developer's real one. `engine::probe` rescues a configured path by its
//! `file_name()` (#74), so `/no/such/place/claude` resolves to
//! `~/.local/bin/claude` and enrichment spawns it with the fixture transcript
//! on stdin. Both names here are chosen so that cannot happen, and
//! `engine::refuse_test_egress` catches the case where a test writes its own
//! anyway.
//!
//! Behind the same `test-guards` feature as that guard, deliberately: the two
//! are one mechanism, and a build where the helper exists is by construction a
//! build where the refusal is live. Neither reaches the shipped daemon.

/// A configured engine path that resolves to nothing on any machine.
///
/// For the tests that assert what happens when an engine is configured and
/// *this* daemon cannot find it. Both halves matter: the directory does not
/// exist, so the path is not used verbatim, and the basename is one no
/// installer writes, so the basename rescue finds nothing either. A bogus
/// directory alone is not enough — that is the bug.
///
/// It is a constant rather than a function so a test can assert on the exact
/// string it configured, which the unresolvable-engine reports do.
pub const UNRESOLVABLE_ENGINE: &str = "/no/such/place/fotw-no-such-engine";

/// The basename for a stub engine a test plants and really runs.
///
/// A stub at a path that exists is safe by construction — `probe` returns a
/// configured path verbatim and never consults the basename — but only for as
/// long as it exists. Name it `claude` and the day the write races the read,
/// or a cleanup runs early, the rescue quietly substitutes the real CLI. This
/// name has nothing to substitute.
pub const STUB_ENGINE_NAME: &str = "fotw-stub-engine";

/// Bail out of a test that pins the #83 refusal when the refusal is switched
/// off.
///
/// `FOTW_ENGINE_LIVE=1` opens the guard on purpose — see
/// [`crate::engine::engine_live_opt_in`] — and a test that asserts the refusal
/// then has nothing left to check. For the fixtures that would go on to
/// *spawn*, this is more than tidiness: with the guard down,
/// `/no/such/place/claude` resolves to the developer's real CLI and the
/// fixture transcript leaves the machine. That is #83 itself, reached through
/// #83's own escape hatch. Those tests have to stop here rather than run their
/// bodies.
///
/// It panics rather than returning because every caller is `#[should_panic]`,
/// and a `should_panic` test has no way to skip. The `#83` marker keeps them
/// satisfied; the message says plainly that nothing was verified — the same
/// bargain `tests/codex_live.rs` makes with its `eprintln!` and early return.
///
/// # Panics
///
/// When `FOTW_ENGINE_LIVE=1`.
pub fn skip_if_engine_live() {
    assert!(
        !crate::engine::engine_live_opt_in(),
        "skipped: FOTW_ENGINE_LIVE=1 opens the #83 guard on purpose, so this \
         test has nothing to check and must not run its body"
    );
}
