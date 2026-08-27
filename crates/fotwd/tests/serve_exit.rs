//! What the log says when the daemon does not start — issue #102.
//!
//! #101 gave the daemon a journal. On the first rebuild after it landed, on a
//! machine whose keychain ACL had never been approved (#53), the whole file
//! read:
//!
//! ```text
//! 2026-08-27T22:21:10Z  daemon   : log opened (rolls at 2048 KiB, one generation kept)
//! 2026-08-27T22:21:10Z  daemon   : serve starting — pid 34369, sessions /…/sessions
//! 2026-08-27T22:22:21Z  daemon   : log opened (rolls at 2048 KiB, one generation kept)
//! 2026-08-27T22:22:21Z  daemon   : serve starting — pid 34760, sessions /…/sessions
//! ```
//!
//! Two fatal startups, and the log says only that they began. The reason
//! existed and was good — it went to the stderr of a LaunchServices-launched
//! `.app`, which macOS discards, which is the whole premise of #101.
//!
//! # Why this is its own test binary
//!
//! [`fotwd::journal::install`] is a process-global `OnceLock`, and
//! `tests/journal.rs` installs one of its own. In one binary whichever test
//! ran first would own the file both then asserted on. A second integration
//! target is a second process, so this one owns its journal outright and can
//! assert on exact lines in order.
//!
//! # What this drives, and what it cannot
//!
//! [`record_exit`] is the seam every `serve` return passes through, and it is
//! driven here for real, against a real journal. `serve` itself is not: its
//! first fallible step is `open_library`, which reads `db:masterkey` from the
//! OS keychain — a test that called it would either raise an approval dialog
//! on the machine running it or build a library under the user's own master
//! key. The failure *reasons* below are therefore reconstructed through the
//! types that produce them, rather than provoked.

#![cfg(unix)]

use fotw_secrets::SecretsError;
use fotwd::journal;
use fotwd::serve::record_exit;

fn tmpdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fotw-serve-exit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sessions")).expect("temp dir");
    dir.join("sessions")
}

fn lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("read")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The refusal that was on that terminal, rebuilt through the error type that
/// produces it and wrapped the way `open_library` hands it back to `serve`.
fn keychain_refusal() -> String {
    let why = SecretsError::Platform {
        operation: "reading",
        key: "db:masterkey".to_owned(),
        detail: "no answer within 60s. This usually means macOS is showing an approval \
                 dialog that nothing can display: the keychain item's ACL is bound to the \
                 code signature that created it, and this binary presents a different one."
            .to_owned(),
    };
    format!("could not open the library: {why}")
}

/// The acceptance criterion of #102: the four-line transcript above gains a
/// line, and that line is the reason.
#[test]
fn a_daemon_that_could_not_start_says_why_on_the_line_after_it_says_it_started() {
    let root = tmpdir();
    let path = journal::install(&root).expect("the journal opens");
    journal::record(&journal::serve_starting(34_369, &root));

    let outcome = record_exit(Err(keychain_refusal()));

    assert!(
        outcome.is_err(),
        "the seam records the outcome, it does not consume it — `main` still \
         prints it to stderr and exits non-zero"
    );

    let written = lines(path);
    assert_eq!(written.len(), 3, "one line per event: {written:?}");
    assert!(written[0].contains("log opened"), "{}", written[0]);
    assert!(written[1].contains("serve starting"), "{}", written[1]);
    assert!(
        written[2].contains("db:masterkey"),
        "the keychain item that could not be read — an account, never material: {}",
        written[2]
    );
    assert!(
        written[2].contains("approval dialog"),
        "and the explanation a person can act on, whole: {}",
        written[2]
    );
    assert!(
        written[2].contains("could not open the library"),
        "and how far the daemon got before it died: {}",
        written[2]
    );

    // The other arm of the same seam. In life these are two processes; the
    // journal is a process-global, so here they are two acts of one test —
    // which also pins that each return writes exactly one line.
    let stopped = record_exit(Ok(()));
    assert!(stopped.is_ok());

    let written = lines(path);
    assert_eq!(written.len(), 4, "{written:?}");
    assert!(
        !written[3].contains('!'),
        "a server that stopped without an error is not a failure: {}",
        written[3]
    );
}
