//! A local MCP (Model Context Protocol) server over the meeting library
//! (MTG-11), so an agent — Claude Desktop, Cursor, Hermes, OpenClaw — can be
//! given context of the user's meetings by querying them live.
//!
//! # Why hand-rolled, and why stdio
//!
//! MCP's stdio transport is newline-delimited JSON-RPC 2.0: one JSON object
//! per line, no embedded newlines. That is small enough to serve directly
//! with `serde_json` — no SDK, no new dependency, the same "MCP-shaped NDJSON
//! JSON-RPC over subprocess stdio" the plugin interface (EXP-08) is specified
//! as. The agent spawns `fotwd mcp` and speaks the protocol down the pipe.
//!
//! # The one rule of a stdio MCP server
//!
//! **Nothing but JSON-RPC may reach stdout.** A stray `println!` corrupts the
//! stream and the client disconnects. Every diagnostic here goes to stderr;
//! [`Server::handle`] returns exactly the bytes that belong on stdout.
//!
//! # What it exposes, and the consent it inherits
//!
//! Three read-only tools over the existing SQLCipher + FTS5 index:
//! `search_meetings`, `get_meeting`, `recent_meetings`. The server reads the
//! encrypted library locally and never writes to it — but whatever the agent
//! retrieves flows to *that agent's* model, which is the user's choice in
//! configuring it. The reading is local; the onward egress is the agent's.

use std::io::{BufRead, Write};

use fotw_store::{Db, SearchQuery};
use serde_json::{Value, json};

/// The MCP protocol version this server defaults to when a client does not
/// name one. A client that does name one has it echoed back, which is what
/// keeps the handshake compatible across the protocol's dated revisions.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// The library-backed MCP server.
pub struct Server {
    db: Db,
}

impl Server {
    /// A server over an open library.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Serve the stdio transport until stdin closes.
    ///
    /// One JSON object per line in, one per line out; notifications produce no
    /// line. A line that does not parse gets a JSON-RPC parse error rather
    /// than taking the server down.
    ///
    /// # Errors
    ///
    /// An I/O error reading stdin or writing stdout.
    pub fn serve_stdio(&mut self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        self.serve(stdin.lock(), stdout.lock())
    }

    /// [`Server::serve_stdio`] over any reader and writer, so the loop's
    /// framing — one response line per request, none for a notification — is
    /// testable without a real pipe.
    ///
    /// # Errors
    ///
    /// An I/O error reading `reader` or writing `writer`.
    pub fn serve<R: BufRead, W: Write>(&mut self, reader: R, mut writer: W) -> std::io::Result<()> {
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(response) = self.handle(&line) {
                writer.write_all(response.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one JSON-RPC message, returning the response line to write, or
    /// `None` for a notification (no `id`) that expects no reply.
    ///
    /// This is the whole protocol as a pure function of the request string and
    /// the library, which is what makes it testable without a pipe.
    #[must_use]
    pub fn handle(&mut self, request: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(request) {
            Ok(v) => v,
            // -32700 Parse error. No id is knowable, so it is null per spec.
            Err(_) => return Some(error_response(&Value::Null, -32700, "parse error")),
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // A message with no id is a notification: act on nothing, answer
        // nothing. `notifications/initialized` is the common one.
        let id = id?;

        match method {
            "initialize" => Some(result_response(&id, self.initialize(&params))),
            "tools/list" => Some(result_response(&id, Self::tools_list())),
            "tools/call" => match self.tools_call(&params) {
                Ok(result) => Some(result_response(&id, result)),
                Err((code, message)) => Some(error_response(&id, code, &message)),
            },
            // `ping` is a liveness check some clients send.
            "ping" => Some(result_response(&id, json!({}))),
            _ => Some(error_response(&id, -32601, "method not found")),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        // Echo the client's protocol version when it names one — the dated
        // revisions are backward compatible for a surface this small, and
        // echoing avoids a version-mismatch disconnect.
        let version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "flyonthewall", "version": env!("CARGO_PKG_VERSION") }
        })
    }

    fn tools_list() -> Value {
        json!({
            "tools": [
                {
                    "name": "search_meetings",
                    "description": "Full-text search the user's meeting transcripts, notes, \
                                    summaries and titles. Returns the best-matching meetings \
                                    with a snippet of the match.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "What to search for." },
                            "limit": { "type": "integer", "description": "Max results (default 10)." }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_meeting",
                    "description": "Get one meeting in full as Markdown: title, date, summary, \
                                    action items and the transcript. Use the meeting_id from \
                                    search_meetings or recent_meetings.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "meeting_id": { "type": "string", "description": "The meeting's id." }
                        },
                        "required": ["meeting_id"]
                    }
                },
                {
                    "name": "recent_meetings",
                    "description": "List the user's most recent meetings, newest first.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "limit": { "type": "integer", "description": "Max results (default 20)." }
                        }
                    }
                }
            ]
        })
    }

    /// Dispatch a `tools/call`. The error half is a JSON-RPC `(code, message)`.
    fn tools_call(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "missing tool name".to_owned()))?;
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);

        let text = match name {
            "search_meetings" => self.tool_search(&args),
            "get_meeting" => self.tool_get_meeting(&args),
            "recent_meetings" => self.tool_recent(&args),
            other => return Err((-32602, format!("unknown tool `{other}`"))),
        };

        match text {
            // A tool-level failure is a normal result with isError, not a
            // protocol error: the agent should see it and adjust, not have the
            // call rejected. This is how MCP models "the tool ran and said no".
            Ok(text) => {
                Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": false }))
            }
            Err(message) => {
                Ok(json!({ "content": [{ "type": "text", "text": message }], "isError": true }))
            }
        }
    }

    fn tool_search(&self, args: &Value) -> Result<String, String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|q| !q.trim().is_empty())
            .ok_or_else(|| "search_meetings needs a non-empty `query`".to_owned())?;
        let limit = args
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(10)
            .clamp(1, 50);

        let hits = self
            .db
            .search(&SearchQuery::new(query).limit(limit))
            .map_err(|_| "the library refused the search".to_owned())?;

        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "meeting_id": h.meeting_id,
                    "title": h.meeting_title,
                    "date": crate::okf::iso_date(u64::try_from(h.started_at_ms).unwrap_or(0)),
                    "matched_in": h.source.as_str(),
                    "snippet": h.snippet,
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&json!({ "results": results })).unwrap_or_default())
    }

    fn tool_get_meeting(&self, args: &Value) -> Result<String, String> {
        let id = args
            .get("meeting_id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "get_meeting needs a `meeting_id`".to_owned())?;
        match self.db.export_meeting(id) {
            Ok(doc) => {
                let mut markdown = doc.to_markdown();
                // An agent reading a meeting with no summary is in the same
                // position the dashboard was in before #74: it cannot tell
                // "nothing worth summarising" from "the engine is broken", and
                // it will happily report the meeting as having no decisions.
                if let Some(note) = enrich_note(&doc) {
                    markdown.push_str("\n> ");
                    markdown.push_str(&note);
                    markdown.push('\n');
                }
                Ok(markdown)
            }
            // A missing meeting is a tool-level "no", not a crash.
            Err(_) => Err(format!("no meeting with id `{id}`")),
        }
    }

    fn tool_recent(&mut self, args: &Value) -> Result<String, String> {
        let limit = args
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(20)
            .clamp(1, 100);
        let meetings = self
            .db
            .meetings()
            .list(limit, 0)
            .map_err(|_| "the library refused to list meetings".to_owned())?;
        let results: Vec<Value> = meetings
            .iter()
            .map(|m| {
                json!({
                    "meeting_id": m.id,
                    "title": if m.title.is_empty() { "Untitled meeting" } else { &m.title },
                    "date": crate::okf::iso_date(u64::try_from(m.started_at_ms).unwrap_or(0)),
                    "state": m.state,
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&json!({ "meetings": results })).unwrap_or_default())
    }
}

/// Why a meeting has no summary, for an agent, or `None` to stay silent.
///
/// Silent when there *is* a summary, and silent when enrichment has not run
/// yet — a meeting that finished four seconds ago is not broken.
fn enrich_note(doc: &fotw_store::MeetingDoc) -> Option<String> {
    if doc.summaries.iter().any(|s| s.is_current != 0) {
        return None;
    }
    match doc.meeting.enrich_status.as_deref()? {
        "no_engine" => Some(
            "No summary: no summarization engine is configured on this machine, so this \
             meeting has its transcript but no derived summary or action items."
                .to_owned(),
        ),
        "engine_unresolvable" => Some(format!(
            "No summary: the configured summarization engine could not be found on this \
             machine ({}), so this meeting has its transcript only.",
            doc.meeting.enrich_detail.as_deref().unwrap_or("unknown")
        )),
        "failed" => Some(format!(
            "No summary: summarization failed for this meeting ({}). The transcript below \
             is complete and unaffected.",
            doc.meeting
                .enrich_detail
                .as_deref()
                .unwrap_or("no reason recorded")
        )),
        _ => None,
    }
}

fn result_response(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_store::{DbKey, NewMeeting, NewSegment};

    fn server_with_a_meeting() -> (Server, String) {
        let mut db = Db::open_in_memory(&DbKey::from_bytes([0x01; 32])).unwrap();
        let id = db
            .meetings()
            .create(
                NewMeeting::new("dev-1", "UTC")
                    .title("Storage migration sync")
                    .started_at_ms(1_755_734_400_000),
            )
            .unwrap();
        let tid = db
            .meetings()
            .create_transcript(&id, "deepgram", "nova-3", true)
            .unwrap();
        db.meetings()
            .append_segments(
                &tid,
                &[NewSegment::new(
                    0,
                    0,
                    1_500,
                    "We decided to move storage to SQLite.",
                )],
            )
            .unwrap();
        db.meetings().set_state(&id, "ready").unwrap();
        (Server::new(db), id)
    }

    fn call(server: &mut Server, req: Value) -> Value {
        let resp = server
            .handle(&req.to_string())
            .expect("a request gets a response");
        serde_json::from_str(&resp).unwrap()
    }

    fn tool_text(server: &mut Server, name: &str, args: Value) -> (String, bool) {
        let resp = call(
            server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
        );
        let content = &resp["result"]["content"][0]["text"];
        let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
        (content.as_str().unwrap_or_default().to_owned(), is_error)
    }

    #[test]
    fn initialize_advertises_tools_and_echoes_the_protocol_version() {
        let (mut s, _) = server_with_a_meeting();
        let resp = call(
            &mut s,
            json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        );
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "flyonthewall");
    }

    #[test]
    fn a_notification_gets_no_response() {
        let (mut s, _) = server_with_a_meeting();
        assert!(
            s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none(),
            "a message with no id expects no reply"
        );
    }

    #[test]
    fn tools_list_names_the_three_tools() {
        let (mut s, _) = server_with_a_meeting();
        let resp = call(
            &mut s,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        );
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["search_meetings", "get_meeting", "recent_meetings"]);
    }

    #[test]
    fn search_finds_a_meeting_by_its_transcript() {
        let (mut s, id) = server_with_a_meeting();
        let (text, is_error) = tool_text(&mut s, "search_meetings", json!({"query":"SQLite"}));
        assert!(!is_error);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["results"][0]["meeting_id"], id);
        assert_eq!(v["results"][0]["title"], "Storage migration sync");
        assert_eq!(v["results"][0]["date"], "2025-08-21");
    }

    #[test]
    fn get_meeting_returns_the_okf_markdown() {
        let (mut s, id) = server_with_a_meeting();
        let (text, is_error) = tool_text(&mut s, "get_meeting", json!({"meeting_id": id}));
        assert!(!is_error);
        assert!(text.contains("type: meeting-transcript"), "OKF frontmatter");
        assert!(text.contains("move storage to SQLite"), "the transcript");
    }

    /// The same blank silence the dashboard had (#74), in the surface an agent
    /// reads. Without this an agent reports "no decisions were made" about a
    /// meeting whose engine was never installed.
    #[test]
    fn a_meeting_with_no_summary_says_which_kind_of_no_summary_it_is() {
        let (mut s, id) = server_with_a_meeting();
        let (clean, _) = tool_text(&mut s, "get_meeting", json!({"meeting_id": id.clone()}));
        assert!(
            !clean.contains("No summary:"),
            "a meeting nothing has reported on stays silent: {clean}"
        );

        s.db.meetings()
            .set_enrich_report(&id, "engine_unresolvable", Some("claude"))
            .unwrap();
        let (text, is_error) = tool_text(&mut s, "get_meeting", json!({"meeting_id": id}));
        assert!(!is_error, "a missing summary is not a tool error");
        assert!(
            text.contains("could not be found on this machine (claude)"),
            "the reason must reach the agent: {text}"
        );
    }

    #[test]
    fn recent_meetings_lists_newest() {
        let (mut s, id) = server_with_a_meeting();
        let (text, is_error) = tool_text(&mut s, "recent_meetings", json!({}));
        assert!(!is_error);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["meetings"][0]["meeting_id"], id);
        assert_eq!(v["meetings"][0]["state"], "ready");
    }

    #[test]
    fn a_missing_meeting_is_a_tool_error_not_a_protocol_error() {
        let (mut s, _) = server_with_a_meeting();
        let (text, is_error) = tool_text(&mut s, "get_meeting", json!({"meeting_id":"nope"}));
        assert!(
            is_error,
            "isError must flag it, but the call still succeeds"
        );
        assert!(text.contains("no meeting"));
    }

    #[test]
    fn an_unknown_method_is_a_jsonrpc_error() {
        let (mut s, _) = server_with_a_meeting();
        let resp = call(&mut s, json!({"jsonrpc":"2.0","id":9,"method":"nonsense"}));
        assert_eq!(resp["error"]["code"], -32601);
        assert_eq!(resp["id"], 9);
    }

    #[test]
    fn garbage_is_a_parse_error_not_a_panic() {
        let (mut s, _) = server_with_a_meeting();
        let resp: Value = serde_json::from_str(&s.handle("{not json").unwrap()).unwrap();
        assert_eq!(resp["error"]["code"], -32700);
    }

    #[test]
    fn the_stdio_loop_answers_requests_and_stays_silent_for_notifications() {
        let (mut s, _) = server_with_a_meeting();
        // A realistic opening: initialize, the initialized notification (no
        // reply), then tools/list. Two requests, one notification → two lines.
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            "\n",
        );
        let mut out = Vec::new();
        s.serve(std::io::Cursor::new(input), &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "one line per request, none for the notification"
        );
        let init: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(init["id"], 0);
        let list: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(list["id"], 1);
        assert!(list["result"]["tools"].is_array());
    }
}
