//! The daemon's own record of what it is doing — issue #101.
//!
//! Every diagnostic the daemon produced went to the stderr of a
//! LaunchServices-launched `.app`, which macOS discards. Asked "is
//! summarization working?", the only way to answer was to kill the running
//! daemon and relaunch it in a terminal — and three wrong conclusions were
//! drawn from outside before anyone did.
//!
//! These pin the parts of the fix that can be tested without a daemon: the
//! file's shape, §10's never-log rule at the two call sites that handle
//! transcript-derived text, and the wording of the daemon's own start and exit
//! lines (#102). `tests/serve_exit.rs` drives that last pair through a real
//! journal, in a process of its own.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;

use fotwd::journal::{
    Journal, Pulse, meeting_problems, meeting_titled, serve_exit, serve_starting,
};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fotw-journal-{name}-{}", std::process::id()));
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

#[test]
fn the_journal_lives_beside_the_audit_log() {
    // `audit.jsonl` is the precedent for "the daemon writes a durable record
    // next to the library", and a second one in a different place is a second
    // place to remember.
    let root = tmpdir("beside");
    let journal = Journal::at(&root);
    let audit = fotwd::audit::AuditLog::at(&root);
    assert_eq!(journal.path().parent(), audit.path().parent());
    assert_eq!(
        journal.path().file_name().and_then(std::ffi::OsStr::to_str),
        Some("fotwd.log")
    );
}

#[test]
fn every_record_is_one_stamped_line_appended_in_order() {
    let root = tmpdir("append");
    let journal = Journal::at(&root);
    for i in 0..3u64 {
        journal
            .record_at(1_700_000_000_000 + i * 1_000, &format!("  line {i}"))
            .expect("write");
    }

    let lines = lines(journal.path());
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "2023-11-14T22:13:20Z  line 0");
    assert_eq!(lines[2], "2023-11-14T22:13:22Z  line 2");
}

#[test]
fn the_journal_is_created_0600_like_every_other_file_under_the_data_root() {
    let root = tmpdir("mode");
    let journal = Journal::at(&root);
    journal
        .record_at(1_700_000_000_000, "hello")
        .expect("write");
    let mode = std::fs::metadata(journal.path())
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the log holds the daemon's diagnostics");
}

#[test]
fn a_message_carrying_newlines_still_costs_exactly_one_line() {
    // A multi-line diagnostic — a sweep report, a provider body — would
    // otherwise turn one record into several, and a reader counting lines
    // would count events that never happened.
    let root = tmpdir("oneline");
    let journal = Journal::at(&root);
    journal
        .record_at(1_700_000_000_000, "first\nsecond\r\nthird")
        .expect("write");

    let lines = lines(journal.path());
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("first second third"), "{}", lines[0]);
}

#[test]
fn the_journal_rolls_at_its_cap_and_keeps_exactly_one_generation() {
    // The daemon runs for weeks. Without this the file is unbounded, and the
    // fix for an unbounded file is someone deleting it at the moment they most
    // want to read it.
    let root = tmpdir("roll");
    let journal = Journal::with_cap(&root, 512);
    for i in 0..200u64 {
        journal
            .record_at(1_700_000_000_000 + i, &format!("line {i:04}"))
            .expect("write");
    }

    let live = std::fs::metadata(journal.path()).expect("stat").len();
    assert!(live <= 512, "the live file must stay under the cap: {live}");
    let rolled = std::fs::metadata(journal.rolled_path())
        .expect("stat")
        .len();
    assert!(rolled > 0, "the previous generation must survive the roll");
    assert!(
        !journal.rolled_path().with_extension("2").exists(),
        "one generation, not a growing pile"
    );

    // The most recent line is always in the live file — the point of a roll is
    // that reading the log still answers "what just happened".
    let last = lines(journal.path()).pop().expect("a live line");
    assert!(last.ends_with("line 0199"), "{last}");
}

#[test]
fn reading_a_journal_that_was_never_written_is_not_an_error() {
    let root = tmpdir("missing");
    assert!(Journal::at(&root).tail(10).expect("empty read").is_empty());
}

#[test]
fn the_tail_is_the_most_recent_lines_newest_last() {
    let root = tmpdir("tail");
    let journal = Journal::at(&root);
    for i in 0..10u64 {
        journal
            .record_at(1_700_000_000_000 + i, &format!("line {i}"))
            .expect("write");
    }
    let tail = journal.tail(3).expect("tail");
    assert_eq!(tail.len(), 3);
    assert!(tail[0].ends_with("line 7"), "{}", tail[0]);
    assert!(tail[2].ends_with("line 9"), "{}", tail[2]);
}

// ------------------------------------------------------- §10's never-log rule

#[test]
fn the_line_written_when_a_meeting_is_titled_never_carries_the_title() {
    // `recording.rs` printed `meeting titled: {title}`, which is fine on a
    // stderr nobody keeps and is a §10 violation the moment it persists:
    // meeting titles are transcript-derived and unreachable from the logging
    // subsystem by rule.
    let title = "Acquisition terms with Northwind, final numbers";
    let note = meeting_titled("2f8c1a4e-0000-4000-8000-000000000001", title);

    assert!(!note.contains(title), "the title itself must not appear");
    assert!(!note.contains("Northwind"), "nor any word of it: {note}");
    assert!(
        note.contains("2f8c1a4e-0000-4000-8000-000000000001"),
        "the id is what makes the line actionable: {note}"
    );
    assert!(
        note.contains("47"),
        "the length is the part worth keeping — a zero-length title is a bug \
         the log has to be able to show: {note}"
    );
}

#[test]
fn the_line_written_when_enrichment_fails_never_quotes_the_problem() {
    // `problems` carries whatever the engine said, and on the CLI arm that is
    // a child process's stderr over a prompt built from the transcript. The
    // detail already has a home the API serves and the dashboard renders —
    // `meetings.enrich_detail` (#74) — so the durable log points at it rather
    // than duplicating something §10 says may not persist.
    let problems = vec![
        "summarize: provider returned HTTP 400: {\"echo\":\"so about the layoffs\"}".to_owned(),
        "title: the engine's reply was not a usable title".to_owned(),
    ];
    let note = meeting_problems("2f8c1a4e-0000-4000-8000-000000000001", &problems);

    assert!(!note.contains("layoffs"), "no engine output may persist");
    assert!(!note.contains("HTTP 400"), "not even the framing: {note}");
    assert!(note.contains('2'), "the count of problems: {note}");
    assert!(
        note.contains("enrich_detail"),
        "and where the detail actually lives: {note}"
    );
}

#[test]
fn a_meeting_with_no_problems_says_so_rather_than_saying_nothing() {
    let note = meeting_problems("2f8c1a4e-0000-4000-8000-000000000001", &[]);
    assert!(note.contains("no problems"), "{note}");
}

// ----------------------------------------- the daemon's own exit — issue #102

#[test]
fn the_starting_line_names_the_process_and_the_library_it_is_opening() {
    // Both halves of "which daemon was this": the pid is what `kill` takes and
    // what tells two launches apart in a rolled log, and the sessions root is
    // which library it opened on a machine that has more than one.
    let line = serve_starting(34_369, std::path::Path::new("/tmp/fotw/sessions"));
    assert!(line.contains("34369"), "{line}");
    assert!(line.contains("/tmp/fotw/sessions"), "{line}");
}

#[test]
fn the_line_written_when_serve_dies_carries_the_reason_it_died_of() {
    // The whole of #102 in one assertion. #101 recorded that the daemon
    // started and never that it failed to, so two fatal startups left four
    // lines that said only that they had begun.
    let line = serve_exit(&Err(
        "could not bind 127.0.0.1: Address already in use (os error 48)".to_owned(),
    ));

    assert!(
        line.contains("Address already in use"),
        "the reason is the entire point of the line: {line}"
    );
    assert!(
        line.contains("could not bind"),
        "and the stage it died at, so the reader knows how far it got: {line}"
    );
    assert!(line.contains('!'), "a fatal exit reads as trouble: {line}");
}

#[test]
fn a_serve_that_stopped_without_an_error_says_that_rather_than_nothing() {
    // "It stopped on its own" and "it was killed from outside" are different
    // facts about a daemon, and a Ctrl-C never reaches this seam — so a log
    // whose last line is `listening` was ended from outside, and one whose
    // last line is this one was not. That only works if the clean exit says
    // something.
    let line = serve_exit(&Ok(()));
    assert!(line.contains("exited"), "{line}");
    assert!(
        !line.contains('!'),
        "and is not dressed up as a failure: {line}"
    );
}

#[test]
fn a_refusal_written_across_eight_lines_still_costs_the_log_one_record() {
    // `lost_key_message` is a paragraph with blank lines and an indented
    // command in it. Through the journal that must stay one record: a reader
    // counting startup attempts would otherwise count eight.
    let root = tmpdir("exit-multiline");
    let journal = Journal::at(&root);
    journal
        .record_at(
            1_700_000_000_000,
            &serve_exit(&Err("could not open the library: there is a library at \
                 /tmp/fotw/db.sqlite3 but its master key is not in this machine's \
                 keychain.\n\n  Your library is recoverable. Run:\n\n      fotwd \
                 recover\n"
                .to_owned())),
        )
        .expect("write");

    let lines = lines(journal.path());
    assert_eq!(lines.len(), 1, "one exit is one record: {lines:?}");
    assert!(
        lines[0].contains("fotwd recover"),
        "and the command that fixes it survives the flattening: {}",
        lines[0]
    );
}

// ------------------------------------------------------------------ the pulse

#[test]
fn a_pulse_repeats_itself_only_when_it_changes_or_an_hour_has_gone_by() {
    // The GitHub pusher wakes once a minute. Logging every wake is 1440 lines
    // a day that drown the ones worth reading; logging none of them is the bug
    // this issue is about.
    let hour = 3_600_000;
    let mut pulse = Pulse::hourly();

    assert!(
        pulse.due(1_000, "nothing owed"),
        "the first one always says"
    );
    assert!(
        !pulse.due(1_000 + 60_000, "nothing owed"),
        "the same answer a minute later is not news"
    );
    assert!(
        pulse.due(1_000 + 120_000, "1 pushed"),
        "a different answer always is"
    );
    assert!(
        !pulse.due(1_000 + 180_000, "1 pushed"),
        "and then settles again"
    );
    assert!(
        pulse.due(1_000 + 120_000 + hour, "1 pushed"),
        "an hour of an unchanged answer is still worth one line — silence and \
         a dead thread must not look the same"
    );
}

// ------------------------------------------------------ the process's own log

/// The end-to-end pin: what the call sites actually write, through the global.
///
/// The one test here that touches `journal::install`, which is a
/// process-global `OnceLock` — everything above uses a `Journal` directly, so
/// nothing in this binary can be affected by the order these run in.
#[test]
fn installing_the_journal_makes_the_call_sites_durable() {
    let root = tmpdir("install");
    let path = fotwd::journal::install(&root).expect("install");
    assert_eq!(fotwd::journal::installed_path(), Some(path));

    fotwd::diag!("  ! could not push meeting {} to GitHub: {}", "abc", "gone");
    fotwd::note!("  pushed     : {} -> {}", "abc", "owner/repo");

    let text = std::fs::read_to_string(path).expect("read");
    assert!(
        text.contains("log opened"),
        "install writes its own first line, so a log that cannot be written \
         says so on the first line rather than the hundredth: {text}"
    );
    assert!(text.contains("! could not push meeting abc to GitHub: gone"));
    assert!(text.contains("pushed     : abc -> owner/repo"));
    assert!(
        text.lines().all(|l| l.starts_with("20")),
        "every line is stamped: {text}"
    );
}
