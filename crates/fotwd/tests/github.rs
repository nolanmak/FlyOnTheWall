//! The GitHub export target (issue #63): the daemon half.
//!
//! Everything here runs against a scripted `gh` — no network, no real binary,
//! no keychain. The scripted runner answers from a queue and records every
//! invocation, so the tests can assert not just the outcome but exactly what
//! would have been executed: `gh` is the process boundary, and what crosses a
//! process boundary is the contract.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotw_web::{GithubError, GithubExport, GithubMode, GithubSettings};
use fotwd::github::{GhOutput, GhRunner, GithubExporter, SETTINGS_KEY};

// ------------------------------------------------------------------ fixtures

fn library() -> Db {
    // The greppable fixed test key, as every store test uses (§10).
    Db::open_in_memory(&DbKey::from_bytes([0x01; 32])).unwrap()
}

/// A finished meeting that enrichment has also run to completion on.
///
/// What the auto pusher owes, after #76. `ready` alone is no longer enough:
/// `persist.rs` sets that state *before* enrichment runs, so a meeting in it
/// may still be waiting for the title and summary the exported Markdown
/// embeds. Every test whose point is "this meeting gets pushed" goes through
/// here; the ones whose point is the gate itself use [`ready_meeting`] raw.
fn enriched_meeting(db: &mut Db, title: &str, started_at_ms: i64) -> String {
    let id = ready_meeting(db, title, started_at_ms);
    mark_enriched(db, &[&id]);
    id
}

/// Stamp meetings as fully enriched, as `enrich_meeting_with` does when a pass
/// reaches its end.
fn mark_enriched(db: &mut Db, meetings: &[&str]) {
    let now = u64::try_from(fotw_store::now_ms()).unwrap_or(0);
    let entries: Vec<(&str, u64, u64)> = meetings.iter().map(|id| (*id, now, now)).collect();
    stamp_receipts(db, &entries);
}

/// Write the enrichment receipt map as **raw JSON**, on `MANUAL`'s precedent.
///
/// Two modules agree on these three field names and neither owns the type, so
/// a rename has to break something loudly. `tests/enrich.rs` pins the same
/// shape from the writing side; this is the reading side.
fn stamp_receipts(db: &mut Db, entries: &[(&str, u64, u64)]) {
    let body: Vec<String> = entries
        .iter()
        .map(|(id, started, finished)| {
            format!(
                r#""{id}":{{"started_at_ms":{started},"finished_at_ms":{finished},"minted_title":""}}"#
            )
        })
        .collect();
    db.put_setting(
        fotwd::enrich::RECEIPTS_KEY,
        &format!("{{{}}}", body.join(",")),
    )
    .unwrap();
}

/// Push a meeting's `updated_at` into the past, the way a library recorded
/// before any of this existed already is.
fn backdate(db: &Db, meeting: &str, updated_at_ms: i64) {
    db.conn()
        .execute(
            &format!("UPDATE meetings SET updated_at = {updated_at_ms} WHERE id = '{meeting}'"),
            [],
        )
        .unwrap();
}

/// A finished meeting with a primary transcript of one segment.
fn ready_meeting(db: &mut Db, title: &str, started_at_ms: i64) -> String {
    let id = db
        .meetings()
        .create(
            NewMeeting::new("dev-1", "UTC")
                .title(title)
                .started_at_ms(started_at_ms),
        )
        .unwrap();
    let tid = db
        .meetings()
        .create_transcript(&id, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(&tid, &[NewSegment::new(0, 0, 1_500, "hello world")])
        .unwrap();
    db.meetings().set_state(&id, "ready").unwrap();
    id
}

/// Settings written straight into the `settings` table, bypassing the stamping
/// logic, so each test controls exactly what the exporter reads back.
fn store_settings(db: &mut Db, json: &str) {
    db.put_setting(SETTINGS_KEY, json).unwrap();
}

const MANUAL: &str = r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"manual"}"#;

/// One recorded invocation: the argv, and what rode in on stdin.
type GhCall = (Vec<String>, Option<Vec<u8>>);

/// A `gh` that answers from a script and remembers what it was asked.
#[derive(Debug, Default)]
struct ScriptedGh {
    script: Mutex<VecDeque<Result<GhOutput, String>>>,
    calls: Mutex<Vec<GhCall>>,
}

impl ScriptedGh {
    fn scripted(steps: Vec<Result<GhOutput, String>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(steps.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<GhCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl GhRunner for ScriptedGh {
    fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Result<GhOutput, String> {
        self.calls
            .lock()
            .unwrap()
            .push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("the script ran out of answers".to_owned()))
    }
}

fn ok(stdout: &str) -> Result<GhOutput, String> {
    Ok(GhOutput {
        status: 0,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    })
}

fn http_err(code: u16) -> Result<GhOutput, String> {
    Ok(GhOutput {
        status: 1,
        stdout: String::new(),
        stderr: format!("gh: Something went wrong (HTTP {code})"),
    })
}

const PUT_OK: &str = r#"{"content":{"path":"x"},"commit":{"sha":"abc123def"}}"#;

/// auth ok → repo ok → no existing file → PUT lands.
fn create_script() -> Vec<Result<GhOutput, String>> {
    vec![
        ok(""),
        ok(r#"{"default_branch":"main"}"#),
        http_err(404),
        ok(PUT_OK),
    ]
}

struct Rig {
    exporter: GithubExporter,
    gh: Arc<ScriptedGh>,
    /// Keeps the temp dir (and the audit log inside it) alive.
    dir: tempfile::TempDir,
    meeting: String,
}

fn rig(settings_json: &str, script: Vec<Result<GhOutput, String>>) -> Rig {
    let mut db = library();
    let meeting = enriched_meeting(&mut db, "Weekly Standup", 1_755_734_400_000); // 2025-08-21 UTC
    store_settings(&mut db, settings_json);
    let gh = ScriptedGh::scripted(script);
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("sessions");
    let exporter = GithubExporter::new(db, root, Arc::clone(&gh) as Arc<dyn GhRunner>);
    Rig {
        exporter,
        gh,
        dir,
        meeting,
    }
}

// ---------------------------------------------------------------- settings

#[test]
fn settings_read_back_what_was_stored_and_default_when_absent() {
    let r = rig(MANUAL, Vec::new());
    let s = r.exporter.settings();
    assert!(s.enabled);
    assert_eq!(s.repo, "octocat/notes");
    assert_eq!(s.mode, GithubMode::Manual);

    let mut db = library();
    ready_meeting(&mut db, "x", 1);
    let fresh = GithubExporter::new(
        db,
        tempfile::TempDir::new().unwrap().path().join("sessions"),
        ScriptedGh::scripted(Vec::new()) as Arc<dyn GhRunner>,
    );
    assert_eq!(
        fresh.settings(),
        GithubSettings::default(),
        "a fresh library is disabled, not an error"
    );
}

#[test]
fn switching_auto_on_stamps_the_moment_and_switching_off_clears_it() {
    let r = rig(MANUAL, Vec::new());

    let auto = GithubSettings {
        enabled: true,
        repo: "octocat/notes".to_owned(),
        mode: GithubMode::Auto,
        ..GithubSettings::default()
    };
    let stored = r.exporter.set_settings(auto.clone()).unwrap();
    let stamp = stored
        .auto_since_ms
        .expect("turning auto on stamps the moment");
    assert!(stamp > 0);

    // Saving again while already auto keeps the original stamp: re-saving the
    // form must not quietly move the "push everything after this" line.
    let again = r.exporter.set_settings(auto).unwrap();
    assert_eq!(again.auto_since_ms, Some(stamp));

    let manual = GithubSettings {
        enabled: true,
        repo: "octocat/notes".to_owned(),
        mode: GithubMode::Manual,
        ..GithubSettings::default()
    };
    let stored = r.exporter.set_settings(manual).unwrap();
    assert_eq!(stored.auto_since_ms, None, "leaving auto clears the stamp");
}

// -------------------------------------------------------------------- push

#[test]
fn a_push_commits_the_markdown_and_saves_the_receipt() {
    let r = rig(MANUAL, create_script());

    let receipt = r
        .exporter
        .push(&r.meeting)
        .expect("the scripted push lands");
    assert_eq!(receipt.repo, "octocat/notes");
    assert_eq!(receipt.commit, "abc123def");
    assert!(
        receipt
            .path
            .starts_with("meetings/2025-08-21-weekly-standup-"),
        "the path is prefix + date + slug + id fragment, got {}",
        receipt.path
    );
    assert!(receipt.path.ends_with(".md"));

    let calls = r.gh.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].0, ["auth", "status", "--hostname", "github.com"]);
    assert_eq!(calls[1].0, ["api", "repos/octocat/notes"]);
    assert_eq!(
        calls[2].0,
        [
            "api",
            &format!("repos/octocat/notes/contents/{}", receipt.path),
            "--jq",
            ".sha"
        ]
    );
    assert_eq!(
        calls[3].0,
        [
            "api",
            "-X",
            "PUT",
            &format!("repos/octocat/notes/contents/{}", receipt.path),
            "--input",
            "-"
        ]
    );

    // What actually crosses the wire: base64 of the EXP-01 Markdown document.
    let body: serde_json::Value = serde_json::from_slice(calls[3].1.as_deref().unwrap()).unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Weekly Standup"),
        "the commit message names the meeting"
    );
    let content = B64.decode(body["content"].as_str().unwrap()).unwrap();
    let markdown = String::from_utf8(content).unwrap();
    assert!(markdown.contains("Weekly Standup"));
    assert!(markdown.contains("hello world"));
    assert!(body["sha"].is_null(), "a first push creates; no sha");
    assert!(
        body["branch"].is_null(),
        "an empty branch setting means the repo default, not a branch named ''"
    );

    // The receipt survives, so a re-push can find its own file again.
    assert_eq!(r.exporter.receipt_for(&r.meeting).unwrap(), receipt);

    // CON-08: the egress is in the audit log.
    let audit = std::fs::read_to_string(r.dir.path().join("audit.jsonl")).unwrap();
    assert!(audit.contains("transcript_pushed"), "audit: {audit}");
    assert!(audit.contains("octocat/notes"));
    assert!(audit.contains(&r.meeting));
    assert!(
        !audit.contains("hello world"),
        "the audit log records the fact of the push, never the content"
    );
}

#[test]
fn a_repush_reuses_the_path_and_sends_the_sha() {
    let r = rig(MANUAL, create_script());
    let first = r.exporter.push(&r.meeting).unwrap();

    // Second run: the file exists now, so the probe answers a sha.
    r.gh.script.lock().unwrap().extend([
        ok(""),
        ok(r#"{"default_branch":"main"}"#),
        ok("oldsha42\n"),
        ok(PUT_OK),
    ]);
    let second = r.exporter.push(&r.meeting).unwrap();
    assert_eq!(
        second.path, first.path,
        "a re-push updates the same file rather than scattering copies"
    );

    let calls = r.gh.calls();
    let body: serde_json::Value = serde_json::from_slice(calls[7].1.as_deref().unwrap()).unwrap();
    assert_eq!(
        body["sha"], "oldsha42",
        "an update names the blob it replaces"
    );
}

#[test]
fn a_configured_branch_rides_on_the_probe_and_the_put() {
    let with_branch = r#"{"enabled":true,"repo":"octocat/notes","branch":"transcripts","path_prefix":"meetings/","mode":"manual"}"#;
    let r = rig(with_branch, create_script());
    r.exporter.push(&r.meeting).unwrap();

    let calls = r.gh.calls();
    assert!(
        calls[2].0[1].ends_with("?ref=transcripts"),
        "the probe must look at the branch it will write to, got {:?}",
        calls[2].0
    );
    let body: serde_json::Value = serde_json::from_slice(calls[3].1.as_deref().unwrap()).unwrap();
    assert_eq!(body["branch"], "transcripts");
}

#[test]
fn each_failure_maps_to_its_own_error() {
    // gh not installed: the very first spawn fails.
    let r = rig(MANUAL, vec![Err("No such file or directory".to_owned())]);
    assert_eq!(r.exporter.push(&r.meeting), Err(GithubError::GhMissing));

    // gh present, nobody logged in.
    let r = rig(
        MANUAL,
        vec![Ok(GhOutput {
            status: 1,
            stdout: String::new(),
            stderr: "You are not logged into any GitHub hosts.".to_owned(),
        })],
    );
    assert_eq!(
        r.exporter.push(&r.meeting),
        Err(GithubError::NotAuthenticated)
    );

    // Logged in, repo gone (or private to someone else — GitHub answers 404
    // for both, deliberately, and so do we).
    let r = rig(MANUAL, vec![ok(""), http_err(404)]);
    assert_eq!(r.exporter.push(&r.meeting), Err(GithubError::RepoNotFound));

    // A token whose scopes cannot see the repo.
    let r = rig(MANUAL, vec![ok(""), http_err(403)]);
    assert_eq!(
        r.exporter.push(&r.meeting),
        Err(GithubError::NotAuthenticated)
    );

    // Disabled: nothing is contacted at all.
    let disabled = r#"{"enabled":false,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"manual"}"#;
    let r = rig(disabled, create_script());
    assert_eq!(r.exporter.push(&r.meeting), Err(GithubError::Disabled));
    assert!(r.gh.calls().is_empty(), "disabled must not spawn a process");

    // A meeting id that names nothing.
    let r = rig(MANUAL, create_script());
    assert_eq!(
        r.exporter.push("01890000-0000-7000-8000-000000000000"),
        Err(GithubError::NoSuchMeeting)
    );
    assert!(r.gh.calls().is_empty(), "no meeting, no process");
}

// -------------------------------------------------------------------- auto

const AUTO_SINCE_EPOCH: &str = r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"auto","auto_since_ms":1}"#;

#[test]
fn auto_push_pushes_a_ready_meeting_exactly_once() {
    let r = rig(AUTO_SINCE_EPOCH, create_script());
    assert_eq!(r.exporter.auto_push_pending(), 1);
    assert_eq!(r.gh.calls().len(), 4);

    // Nothing new: the receipt is the memory.
    assert_eq!(r.exporter.auto_push_pending(), 0);
    assert_eq!(
        r.gh.calls().len(),
        4,
        "an already-pushed meeting stays pushed"
    );
}

// -------------------------------------------- waiting for enrichment (#76)

/// The race #76 exists to close, from the exporter's side.
///
/// `persist.rs` sets `state = "ready"` six seconds before enrichment even
/// opens the keychain, and the pusher polls that state every sixty seconds. It
/// won a real meeting on 2026-08-25: the file landed in the repo under
/// `untitled-recording-1787677095` while the library row was still being
/// titled, and the receipt pinned that path forever.
///
/// Not fixed by re-sequencing `set_state`: that strands a meeting whose
/// enrichment dies before touching the library, delays the retention clocks
/// that key on `ready`, and mutates a state enum the UI and MCP both read.
#[test]
fn a_meeting_whose_enrichment_has_not_finished_is_not_exported_yet() {
    let mut db = library();
    // Ready, and *not* stamped — the six-second window, exactly.
    ready_meeting(
        &mut db,
        "Untitled recording — 2026-08-25 14:05 UTC",
        1_755_734_400_000,
    );
    store_settings(&mut db, AUTO_SINCE_EPOCH);
    let gh = ScriptedGh::scripted(create_script());
    let dir = tempfile::TempDir::new().unwrap();
    let exporter = GithubExporter::new(
        db,
        dir.path().join("sessions"),
        Arc::clone(&gh) as Arc<dyn GhRunner>,
    );

    assert_eq!(exporter.auto_push_pending(), 0);
    assert!(
        gh.calls().is_empty(),
        "a meeting still being enriched must not reach a process at all"
    );
}

/// And the other side of it: once enrichment has finished, the file lands
/// under the title enrichment gave it, which is what #67 asked for.
#[test]
fn an_enriched_meeting_exports_under_the_title_enrichment_gave_it() {
    let r = rig(AUTO_SINCE_EPOCH, create_script());
    assert_eq!(r.exporter.auto_push_pending(), 1);
    let receipt = r.exporter.receipt_for(&r.meeting).expect("it pushed");
    assert!(
        receipt.path.contains("weekly-standup"),
        "the path must carry the real title's slug, not `untitled-recording`: {}",
        receipt.path
    );
}

/// The valve. Enrichment that never finishes — the daemon was killed mid-pass,
/// or the keychain refused before a stamp could be written — must not hold a
/// meeting back forever. The window is measured from the moment enrichment
/// *began*, because a long meeting spends a title call plus a Call B per chunk
/// inside it and a window anchored at persist time expires mid-run.
#[test]
fn an_enrichment_that_never_finished_exports_once_the_grace_expires() {
    let mut db = library();
    let meeting = ready_meeting(&mut db, "Weekly Standup", 1_755_734_400_000);
    let long_ago = u64::try_from(fotw_store::now_ms()).unwrap_or(0) - 3_600_000;
    stamp_receipts(&mut db, &[(&meeting, long_ago, 0)]); // started, never finished
    store_settings(&mut db, AUTO_SINCE_EPOCH);
    let dir = tempfile::TempDir::new().unwrap();
    let exporter = GithubExporter::new(
        db,
        dir.path().join("sessions"),
        ScriptedGh::scripted(create_script()) as Arc<dyn GhRunner>,
    );

    assert_eq!(
        exporter.auto_push_pending(),
        1,
        "an hour past the start of a pass that never ended, the meeting is owed"
    );
}

/// A library recorded before any stamp existed has no receipts at all, and
/// every one of its meetings is long finished. It must export immediately
/// rather than wait out a grace window for a pass that will never run.
#[test]
fn a_library_from_before_the_stamp_existed_still_exports() {
    let mut db = library();
    let meeting = ready_meeting(&mut db, "Weekly Standup", 1_755_734_400_000);
    backdate(&db, &meeting, fotw_store::now_ms() - 86_400_000);
    store_settings(&mut db, AUTO_SINCE_EPOCH);
    let dir = tempfile::TempDir::new().unwrap();
    let exporter = GithubExporter::new(
        db,
        dir.path().join("sessions"),
        ScriptedGh::scripted(create_script()) as Arc<dyn GhRunner>,
    );

    assert_eq!(exporter.auto_push_pending(), 1);
}

/// The gate is on the *worker*, never on the person. A manual push is a human
/// saying "export this now", and it has never had a state check at all.
#[test]
fn a_manual_push_is_never_held_back_by_the_gate() {
    let mut db = library();
    let meeting = ready_meeting(&mut db, "Weekly Standup", 1_755_734_400_000);
    store_settings(&mut db, MANUAL);
    let dir = tempfile::TempDir::new().unwrap();
    let exporter = GithubExporter::new(
        db,
        dir.path().join("sessions"),
        ScriptedGh::scripted(create_script()) as Arc<dyn GhRunner>,
    );

    assert!(
        exporter.push(&meeting).is_ok(),
        "an unenriched meeting is still the user's to export by hand"
    );
}

#[test]
fn auto_push_skips_meetings_from_before_the_stamp() {
    // The stamp is far in the future of the fixture meeting.
    let late = r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"auto","auto_since_ms":9999999999999}"#;
    let r = rig(late, create_script());
    assert_eq!(
        r.exporter.auto_push_pending(),
        0,
        "enabling auto must not export the archive"
    );
    assert!(r.gh.calls().is_empty());
}

#[test]
fn a_meeting_that_fails_on_its_own_is_parked_until_restart() {
    // The PUT itself is refused — something about *this* meeting.
    let r = rig(
        AUTO_SINCE_EPOCH,
        vec![
            ok(""),
            ok(r#"{"default_branch":"main"}"#),
            http_err(404),
            http_err(422),
        ],
    );
    assert_eq!(r.exporter.auto_push_pending(), 0);
    let after_first = r.gh.calls().len();
    assert_eq!(after_first, 4);

    assert_eq!(r.exporter.auto_push_pending(), 0);
    assert_eq!(
        r.gh.calls().len(),
        after_first,
        "a meeting that keeps failing must not hammer gh once a minute"
    );
}

/// gh missing, nobody logged in, repo unreachable — none of these are the
/// meeting's fault. Fixing the environment must drain the backlog without a
/// daemon restart, which is the promise `SystemGh` re-resolving per call
/// already makes.
#[test]
fn an_environment_failure_is_retried_and_stops_the_round() {
    let mut db = library();
    let first = ready_meeting(&mut db, "First", 1_755_734_400_000);
    let second = ready_meeting(&mut db, "Second", 1_755_734_500_000);
    mark_enriched(&mut db, &[&first, &second]);
    store_settings(&mut db, AUTO_SINCE_EPOCH);
    let gh = ScriptedGh::scripted(vec![Ok(GhOutput {
        status: 1,
        stdout: String::new(),
        stderr: "You are not logged into any GitHub hosts.".to_owned(),
    })]);
    let dir = tempfile::TempDir::new().unwrap();
    let exporter = GithubExporter::new(
        db,
        dir.path().join("sessions"),
        Arc::clone(&gh) as Arc<dyn GhRunner>,
    );

    assert_eq!(exporter.auto_push_pending(), 0);
    assert_eq!(
        gh.calls().len(),
        1,
        "one failed login answers for every meeting — the round must stop, \
         not repeat the same refusal per meeting"
    );

    // The environment is fixed; the next poll owes both meetings.
    gh.script.lock().unwrap().extend(create_script());
    gh.script.lock().unwrap().extend(create_script());
    assert_eq!(
        exporter.auto_push_pending(),
        2,
        "meetings {first} and {second} must not be parked by an environment failure"
    );
}

/// Auto with no stamp would mean "everything, ever". The guard refusing that
/// is the only thing between a hand-edited settings row and a full-archive
/// export.
#[test]
fn auto_mode_with_no_stamp_pushes_nothing() {
    let stampless = r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"auto"}"#;
    let r = rig(stampless, create_script());
    assert_eq!(r.exporter.auto_push_pending(), 0);
    assert!(r.gh.calls().is_empty());
}

/// The park list gates the *worker*, never the person: a manual push is a
/// human saying "try again now", and it must actually try.
#[test]
fn a_manual_push_retries_a_parked_meeting() {
    let r = rig(
        AUTO_SINCE_EPOCH,
        vec![
            ok(""),
            ok(r#"{"default_branch":"main"}"#),
            http_err(404),
            http_err(422),
        ],
    );
    assert_eq!(r.exporter.auto_push_pending(), 0, "parked");

    r.gh.script.lock().unwrap().extend(create_script());
    let receipt = r
        .exporter
        .push(&r.meeting)
        .expect("a manual push ignores the park list");
    assert_eq!(receipt.repo, "octocat/notes");
}

/// A `gh` that answers the create sequence forever, for tests whose point is
/// volume rather than the exact exchange.
#[derive(Debug, Default)]
struct TirelessGh {
    calls: Mutex<usize>,
}

impl GhRunner for TirelessGh {
    fn run(&self, _args: &[String], _stdin: Option<&[u8]>) -> Result<GhOutput, String> {
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        match (*n - 1) % 4 {
            2 => http_err(404),
            _ => ok(PUT_OK),
        }
    }
}

/// One `list` page is 200 meetings. A backlog deeper than that must still
/// drain — the busiest imaginable library must not silently strand its
/// oldest owed meeting.
#[test]
fn auto_push_reaches_meetings_beyond_the_first_page() {
    let mut db = library();
    let ids: Vec<String> = (0..201)
        .map(|i| ready_meeting(&mut db, &format!("m{i}"), 1_755_734_400_000 + i))
        .collect();
    // One write for all 201: the map is a single settings row, and stamping it
    // per meeting would be 201 read-modify-writes of a growing JSON object.
    mark_enriched(&mut db, &ids.iter().map(String::as_str).collect::<Vec<_>>());
    store_settings(&mut db, AUTO_SINCE_EPOCH);
    let dir = tempfile::TempDir::new().unwrap();
    let exporter = GithubExporter::new(
        db,
        dir.path().join("sessions"),
        Arc::new(TirelessGh::default()) as Arc<dyn GhRunner>,
    );
    assert_eq!(exporter.auto_push_pending(), 201);
}

#[test]
fn manual_mode_never_pushes_on_its_own() {
    let r = rig(MANUAL, create_script());
    assert_eq!(r.exporter.auto_push_pending(), 0);
    assert!(r.gh.calls().is_empty());
}

// -------------------------------------------------------------------- repos

/// The settings form's picker: one `gh api` call, only repos this login can
/// push to, most recently active first — the order GitHub already answers in.
#[test]
fn the_repo_picker_asks_gh_for_pushable_repos() {
    let r = rig(MANUAL, vec![ok(r#"["octocat/notes","work-org/minutes"]"#)]);
    let repos = r.exporter.repos().expect("the scripted listing answers");
    assert_eq!(repos, ["octocat/notes", "work-org/minutes"]);

    let calls = r.gh.calls();
    assert_eq!(calls.len(), 1, "one subprocess, no preflight ceremony");
    assert_eq!(
        calls[0].0,
        [
            "api",
            "user/repos?per_page=100&sort=pushed",
            "--jq",
            "[.[] | select(.permissions.push) | .full_name]"
        ]
    );
}

#[test]
fn the_repo_picker_maps_failures_like_a_push_does() {
    let r = rig(MANUAL, vec![Err("No such file or directory".to_owned())]);
    assert_eq!(r.exporter.repos(), Err(GithubError::GhMissing));

    let r = rig(MANUAL, vec![http_err(401)]);
    assert_eq!(r.exporter.repos(), Err(GithubError::NotAuthenticated));

    // Garbage stdout is a Failed, never a panic and never an empty success.
    let r = rig(MANUAL, vec![ok("not json")]);
    assert!(matches!(r.exporter.repos(), Err(GithubError::Failed(_))));
}

// -------------------------------------------------------------- OKF bundle

/// The six-call sync sequence: preflight (auth, repo) then create index.md and
/// log.md (each a probe + PUT).
fn sync_script() -> Vec<Result<GhOutput, String>> {
    vec![
        ok(""),
        ok(r#"{"default_branch":"main"}"#),
        http_err(404),
        ok(PUT_OK),
        http_err(404),
        ok(PUT_OK),
    ]
}

#[test]
fn sync_bundle_commits_an_okf_index_and_log() {
    let r = rig(MANUAL, create_script());
    // A push first, so there is a receipt to index.
    r.exporter.push(&r.meeting).unwrap();
    let after_push = r.gh.calls().len();

    r.gh.script.lock().unwrap().extend(sync_script());
    GithubExport::sync_bundle(&r.exporter).expect("the scripted sync lands");

    let calls = r.gh.calls();
    assert_eq!(calls.len(), after_push + 6, "auth+repo, then index and log");

    // index.md — OKF listing with a relative, dated link to the meeting.
    let index_put = &calls[after_push + 3];
    assert_eq!(
        index_put.0,
        [
            "api",
            "-X",
            "PUT",
            "repos/octocat/notes/contents/meetings/index.md",
            "--input",
            "-"
        ]
    );
    let body: serde_json::Value = serde_json::from_slice(index_put.1.as_deref().unwrap()).unwrap();
    let index_md =
        String::from_utf8(B64.decode(body["content"].as_str().unwrap()).unwrap()).unwrap();
    assert!(
        index_md.contains("okf_version"),
        "OKF frontmatter: {index_md}"
    );
    assert!(
        index_md.contains("[Weekly Standup](./2025-08-21-weekly-standup-"),
        "relative dated link missing: {index_md}"
    );

    // log.md — a change-history entry under an ISO date heading.
    let log_put = &calls[after_push + 5];
    assert_eq!(log_put.0[3], "repos/octocat/notes/contents/meetings/log.md");
    let lbody: serde_json::Value = serde_json::from_slice(log_put.1.as_deref().unwrap()).unwrap();
    let log_md =
        String::from_utf8(B64.decode(lbody["content"].as_str().unwrap()).unwrap()).unwrap();
    assert!(log_md.contains("# Change log"));
    assert!(log_md.contains("- Added [Weekly Standup](./2025-08-21-weekly-standup-"));
}

#[test]
fn sync_bundle_with_nothing_pushed_contacts_no_gh() {
    // Enabled, but no receipts yet: there is no bundle to write.
    let r = rig(MANUAL, sync_script());
    GithubExport::sync_bundle(&r.exporter).expect("a no-op is not a failure");
    assert!(
        r.gh.calls().is_empty(),
        "nothing to index, nothing to spawn"
    );
}

#[test]
fn sync_bundle_is_refused_when_export_is_disabled() {
    let disabled = r#"{"enabled":false,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"manual"}"#;
    let r = rig(disabled, sync_script());
    assert_eq!(
        GithubExport::sync_bundle(&r.exporter),
        Err(GithubError::Disabled)
    );
    assert!(r.gh.calls().is_empty());
}
