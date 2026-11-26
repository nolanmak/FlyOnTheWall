//! Attendee release never touches the GitHub archive (#326 D1, N1, §9).
//!
//! The same meeting is pushed with release off, and again after release has
//! been switched on with a prepared row, decisions, an eligibility entry and
//! a ledger entry in the library. The files the archive receives must be
//! byte-identical. Scripted `gh` only; synthetic data.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use fotw_store::{Db, DbKey, NewMeeting, NewSegment};
use fotw_web::GithubExport;
use fotwd::github::{GhOutput, GhRunner, GithubExporter, SETTINGS_KEY};

const KEY: [u8; 32] = [0x01; 32];
const MANUAL: &str = r#"{"enabled":true,"repo":"octocat/notes","branch":"","path_prefix":"meetings/","mode":"manual"}"#;
const PRIVATE_REPO: &str = r#"{"default_branch":"main","private":true,"visibility":"private"}"#;
const PUT_OK: &str = r#"{"content":{"path":"x"},"commit":{"sha":"abc123def"}}"#;

type GhCall = (Vec<String>, Option<Vec<u8>>);

#[derive(Default)]
struct ScriptedGh {
    script: Mutex<VecDeque<Result<GhOutput, String>>>,
    calls: Mutex<Vec<GhCall>>,
}

impl ScriptedGh {
    fn push_script(&self, steps: Vec<Result<GhOutput, String>>) {
        self.script.lock().unwrap().extend(steps);
    }
    fn last_put(&self) -> serde_json::Value {
        let calls = self.calls.lock().unwrap();
        let (_, stdin) = calls
            .iter()
            .rev()
            .find(|(args, _)| args.iter().any(|a| a == "PUT"))
            .expect("a PUT was made");
        serde_json::from_slice(stdin.as_deref().unwrap()).unwrap()
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

fn not_found() -> Result<GhOutput, String> {
    Ok(GhOutput {
        status: 1,
        stdout: String::new(),
        stderr: "gh: Not Found (HTTP 404)".to_owned(),
    })
}

/// What reaches the archive: the file path, its bytes and the commit message.
fn archived(put: &serde_json::Value) -> (String, Vec<u8>, String) {
    (
        put["message"].as_str().unwrap_or_default().to_owned(),
        B64.decode(put["content"].as_str().unwrap()).unwrap(),
        put["branch"].to_string(),
    )
}

#[test]
fn github_export_is_byte_identical_with_release_on_and_off() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("library/db.sqlite3");
    let key = DbKey::from_bytes(KEY);
    let mut db = Db::open(&path, &key).unwrap();
    let id = db
        .meetings()
        .create(
            NewMeeting::new("dev-1", "UTC")
                .title("Planning sync")
                .started_at_ms(1_755_734_400_000),
        )
        .unwrap();
    let tid = db
        .meetings()
        .create_transcript(&id, "deepgram", "nova-3", true)
        .unwrap();
    db.meetings()
        .append_segments(&tid, &[NewSegment::new(0, 0, 1_500, "we agreed the plan")])
        .unwrap();
    db.meetings().set_state(&id, "ready").unwrap();
    db.put_setting(SETTINGS_KEY, MANUAL).unwrap();

    // A second connection, as the release worker has its own.
    let side = Db::open(&path, &key).unwrap();

    let gh = Arc::new(ScriptedGh::default());
    let exporter = GithubExporter::new(
        db,
        dir.path().join("sessions"),
        Arc::clone(&gh) as Arc<dyn GhRunner>,
    );

    gh.push_script(vec![ok(""), ok(PRIVATE_REPO), not_found(), ok(PUT_OK)]);
    let first = exporter.push(&id).expect("push with release off");
    let off = archived(&gh.last_put());

    side.conn()
        .execute_batch(&format!(
            r#"
INSERT INTO app_meta (key, value, updated_at) VALUES
  ('release_settings', '{{"enabled":true,"allowed_domains":["example.com"],"verified_sender":"owner@example.com"}}', 1),
  ('release_notice_last_ms', '1', 1),
  ('release_attention_last_ms', '1', 1);
INSERT INTO meeting_releases (meeting_id, state, reasons_json, source, source_event_id,
  event_title, recipients_json, rendered_subject, rendered_body, rendered_attachment,
  created_at, updated_at)
VALUES ('{id}', 'needs_review', '["review_required"]', 'eventkit', 'evt-1',
  'Planning sync', '[{{"address":"ana@example.com","display_name":null,"response":"accepted","is_self":true}}]',
  'Notes: Planning sync (2025-08-21)', 'Summary', '# Notes', 1, 1);
INSERT INTO release_decisions (id, meeting_id, engine_id, decision, at_ms)
VALUES ('dec-1', '{id}', 'engine', 'approved_unchanged', 1);
INSERT INTO release_sent_addresses (address, first_sent_ms) VALUES ('ana@example.com', 1);
"#
        ))
        .unwrap();

    gh.push_script(vec![ok(""), ok(PRIVATE_REPO), ok("abc123def"), ok(PUT_OK)]);
    let second = exporter.push(&id).expect("push with release on");
    let on = archived(&gh.last_put());

    assert_eq!(first.path, second.path, "the same file in the archive");
    assert_eq!(off, on, "release state changed what the archive receives");
}
