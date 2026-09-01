//! The embedded SPA, and the headers it is served with (ING-11).
//!
//! # Why `debug-embed` is not optional
//!
//! `rust_embed` reads from disk in debug builds unless `debug-embed` is on.
//! That default is a nice edit-refresh loop and a very bad shipping failure:
//! the developer's machine has `crates/fotw-web/ui/` next to the binary and
//! works perfectly, every test passes, and the notarised `.app` on a user's
//! machine 404s the entire UI — because the folder the macro recorded is an
//! absolute path into a build directory that no longer exists. The feature is
//! declared in `Cargo.toml` with this note attached; it is not a preference.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;

use crate::ingress::not_found;
use crate::state::AppState;

/// Everything under `crates/fotw-web/ui/`, compiled into the binary.
#[derive(rust_embed::Embed)]
#[folder = "ui/"]
struct Ui;

/// `GET /` — the SPA shell.
pub async fn index(State(state): State<AppState>) -> Response {
    serve(&state, "index.html")
}

/// `GET /assets/{*path}` — the shell's script and stylesheet.
pub async fn asset(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    serve(&state, &path)
}

fn serve(state: &AppState, path: &str) -> Response {
    // A miss is the same bare 404 as an unauthorised request (ING-09), so a
    // caller cannot enumerate what the bundle contains. Traversal is not a
    // concern in the first place — `Ui::get` is a lookup in a compile-time
    // map, not a filesystem open — but `..` is refused explicitly so that a
    // future move to disk-backed assets does not silently become a file read.
    if path.contains("..") {
        return not_found();
    }
    // A developer override, when one is active, replaces where a known asset
    // comes from — never what the server exposes. See `dev_override`.
    if let Some(bytes) = dev_override(path) {
        return asset_response(state, path, bytes);
    }
    let Some(file) = Ui::get(path) else {
        return not_found();
    };
    asset_response(state, path, file.data.into_owned())
}

/// The one way an asset body becomes a response.
///
/// Shared by the embedded and the override paths so their headers cannot
/// drift: `tests/ingress.rs` compares responses byte for byte, and a header
/// that existed only on one path would make the two distinguishable.
fn asset_response(state: &AppState, path: &str, bytes: Vec<u8>) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(bytes))
        .expect("a static asset response is always constructible");
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type(path));
    apply_security_headers(headers, state.csp());
    response
}

/// `FOTW_UI_DIR` — read a known asset from disk instead of the embedded copy.
///
/// The embed is forced on even in debug (`debug-embed`) because silently
/// reading from disk in dev is how a shipped bundle 404s its whole UI on a
/// machine with no `ui/` directory. That hazard was about *silence*: an env
/// var someone typed is not silent, so the escape is explicit — and it is
/// compiled out of release entirely, so the original failure mode cannot
/// return no matter what is in the environment.
///
/// Without it, the UI iteration loop is edit → `cargo build` → `just
/// dev-sign` → a new binary identity → a keychain approval dialog → relaunch,
/// for a one-character CSS change.
#[cfg(debug_assertions)]
fn dev_override(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var_os("FOTW_UI_DIR")?;
    dev_override_from(std::path::Path::new(&dir), name)
}

/// A release binary ignores the variable unconditionally.
#[cfg(not(debug_assertions))]
fn dev_override(_name: &str) -> Option<Vec<u8>> {
    None
}

/// [`dev_override`] with the directory named, so a test needs no env var —
/// the environment is process-global and tests run in parallel.
#[cfg(debug_assertions)]
fn dev_override_from(dir: &std::path::Path, name: &str) -> Option<Vec<u8>> {
    // The allowlist is the embedded bundle itself: only a name that ships can
    // be overridden, so the join below cannot be steered anywhere new and the
    // override cannot widen what the server exposes. A miss — unknown name or
    // missing file — falls back to the embedded copy, because half a
    // directory is a normal state while editing.
    if !Ui::iter().any(|f| f == name) {
        return None;
    }
    std::fs::read(dir.join(name)).ok()
}

/// The headers every non-404 response carries.
///
/// Applied here and in [`crate::api`] rather than in a `tower` layer over the
/// whole router, because a layer that decorated responses would decorate the
/// fallback's 404 too — and then a rejected request's 404 (which gets no
/// decoration, because it never reaches a handler) would be distinguishable
/// from an unknown path's 404 by its headers alone. ING-09 asks for the two to
/// be byte-identical, so nothing may touch a 404 on its way out.
pub fn apply_security_headers(headers: &mut axum::http::HeaderMap, csp: &str) {
    if let Ok(value) = HeaderValue::from_str(csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // ING-10: the launch URL carries the handoff token in its query string.
    // Without this, the first request the page makes to any other origin would
    // carry that URL in `Referer`. `no-referrer` also covers the case the SPA
    // cannot control — an `<img>` a transcript somehow injected.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // Belt and braces with `frame-ancestors 'none'`, for the CSP-blind.
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Meeting transcripts must not land in the browser's disk cache, where
    // they outlive both the daemon and its per-start secret.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
}

fn content_type(path: &str) -> HeaderValue {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or_default();
    HeaderValue::from_static(match ext {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/vnd.microsoft.icon",
        // Deliberately not `text/html`: an unknown asset type rendered as HTML
        // is a stored-XSS primitive, and `nosniff` plus this makes the browser
        // download it instead of running it.
        _ => "application/octet-stream",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One embedded asset, as text.
    fn asset_text(name: &str) -> String {
        let file = Ui::get(name).expect("the asset is embedded");
        String::from_utf8(file.data.into_owned()).expect("the bundle is UTF-8")
    }

    /// The body of one CSS rule, so a pin can assert what a class is *styled*
    /// with rather than that its name appears somewhere in the file.
    fn css_rule<'a>(css: &'a str, selector: &str) -> &'a str {
        let head = format!("\n{selector} {{");
        assert!(css.contains(&head), "app.css has no `{selector}` rule");
        let body = &css[css.find(&head).unwrap() + head.len()..];
        &body[..body.find('}').unwrap()]
    }

    /// CON-02's red, spelled once. `.recording` is the only rule that may
    /// carry it: it means audio is being captured right now.
    const INDICATOR_RED: &str = "#d2453d";

    /// The `debug-embed` failure, caught at test time rather than at
    /// notarisation time. If the feature were off and the folder missing, this
    /// is the assertion that would fail.
    #[test]
    fn the_shell_and_its_assets_are_embedded() {
        for name in ["index.html", "app.js", "app.css"] {
            assert!(Ui::get(name).is_some(), "{name} must be in the binary");
        }
    }

    #[test]
    fn the_shell_loads_no_remote_anything() {
        let html = Ui::get("index.html").unwrap();
        let html = String::from_utf8(html.data.into_owned()).unwrap();
        for forbidden in ["http://", "https://", "//cdn", "integrity="] {
            assert!(
                !html.contains(forbidden),
                "the shell must reference nothing off this origin, found {forbidden}"
            );
        }
    }

    /// ING-11 is only as good as the renderer it backs up. The CSP catches an
    /// `innerHTML`; this catches it earlier, in review.
    #[test]
    fn the_renderer_never_assigns_html() {
        let js = Ui::get("app.js").unwrap();
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        for forbidden in ["innerHTML", "outerHTML", "insertAdjacentHTML", "eval("] {
            assert!(
                !js.contains(forbidden),
                "transcript text is attacker-influenced; {forbidden} must not \
                 appear in the SPA"
            );
        }
    }

    /// The one pin `app.js` can have: there is no JS harness in this project,
    /// so nothing else in the suite would notice the client losing the third
    /// recording state and going back to a clock that climbs past Stop (#77).
    /// A grep is weak, and it is not nothing.
    #[test]
    fn the_spa_knows_about_the_finishing_state() {
        let js = Ui::get("app.js").unwrap();
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("\"finishing\""),
            "the SPA must switch on the daemon's finishing state, or Stop \
             leaves the session clock running"
        );
    }

    /// The same weak-but-not-nothing pin for #78. The server half of
    /// `meeting_ready` has real tests either side of the socket; the client
    /// half has only this, and a handler that never learned the frame's name
    /// is a library that goes stale until the tab is reloaded — which is the
    /// entire bug.
    #[test]
    fn the_spa_acts_on_the_meeting_ready_frame() {
        let js = Ui::get("app.js").unwrap();
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("\"meeting_ready\""),
            "the SPA must handle the meeting_ready frame, or a finished \
             meeting appears in the library only after a reload"
        );
    }

    /// The third pin of the same weak kind, for #90. `renderMarkdown` knew
    /// bullets, headings and paragraphs, so the daemon's admonitions rendered
    /// as two lines of `>`-prefixed source at the very top of the pane. The
    /// *shape* of that parse is tested for real one crate over, on
    /// `fotw_store`'s clipboard converter, which renders the same subset of
    /// the same `body_md`; this only catches the client losing the branch.
    #[test]
    fn the_spa_renders_admonitions_as_callouts_not_as_source() {
        let js = Ui::get("app.js").unwrap();
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("blockquote"),
            "the SPA must render `>` lines as a blockquote, or a summary's \
             warnings read as markdown source"
        );
        assert!(
            js.contains("[!WARNING]"),
            "the SPA must know the admonition markers the daemon emits, or \
             the marker line renders as literal text inside the callout"
        );
    }

    /// The fourth pin of the same weak kind, for #91. A meeting that has
    /// stopped capturing and is being written to disk was rendering in
    /// `.recording` — the exact red that means audio is arriving *right now*
    /// — because #77 gave the recorder a third state and left the badge on
    /// the first one's class. #90 added `--caution` for precisely this class
    /// of "important, but not that".
    ///
    /// A colour cannot be asserted without a browser, so this asserts the two
    /// halves that decide it: the client gives the badge a class per state,
    /// and that class is styled in the caution token rather than in the
    /// indicator's red.
    #[test]
    fn the_finishing_badge_is_not_dressed_as_the_recording_indicator() {
        let js = asset_text("app.js");
        let css = asset_text("app.css");
        assert!(
            js.contains("el.recording.className"),
            "the badge must take its class from the state, or finishing keeps \
             CON-02's red over a meeting whose taps are already closed"
        );
        let finishing = css_rule(&css, ".finishing");
        assert!(
            finishing.contains("var(--caution)"),
            "the finishing badge is the caution colour (#90's token): {finishing}"
        );
        assert!(
            !finishing.contains(INDICATOR_RED),
            "CON-02's red means capture is live and means nothing else: {finishing}"
        );
        // The other direction, and the one that matters: this narrows what the
        // red covers, it does not weaken the red.
        assert!(
            css_rule(&css, ".recording").contains(INDICATOR_RED),
            "the live-capture badge must keep CON-02's red"
        );
    }

    /// The fifth, for #91's other half. Stopping a recording started on the
    /// dashboard left the pane showing a "Recording" header over a meeting
    /// that was already in the library: `showLive` sets `currentDetail =
    /// null`, so #78's re-open — which matches the frame's id against the
    /// open pane — had nothing to match against.
    ///
    /// The guard is the half worth pinning. `currentDetail` is null both when
    /// the live pane is up and when nothing has been opened at all, so the
    /// client needs some other way to know it is replacing its own live pane
    /// rather than a meeting someone chose to read.
    #[test]
    fn the_spa_opens_the_meeting_a_finished_recording_became() {
        let js = asset_text("app.js");
        assert!(
            js.contains("openMeeting(frame.meeting_id)"),
            "the SPA must open the meeting the frame names, or the pane keeps \
             the live header until the user clicks something"
        );
        assert!(
            js.contains("\"persisted\""),
            "the SPA must branch on which of the two meeting_ready moments \
             this is, or a later frame reopens a pane the user has moved on \
             from"
        );
        assert!(
            js.contains("live-pane"),
            "the live pane needs a marker the client can look for: \
             `currentDetail` is null both when it is showing and when nothing \
             has been opened at all"
        );
    }

    /// The live transcript should follow new words only while the reader is
    /// at the bottom. This is a client interaction, so the asset test pins
    /// the small contract that protects it until the project has a browser
    /// harness: measure before appending, then restore the bottom only when
    /// that measurement says the reader was already following.
    #[test]
    fn the_live_transcript_follows_the_bottom_without_fighting_manual_scroll() {
        let js = asset_text("app.js");
        assert!(
            js.contains("const LIVE_SCROLL_SLOP_PX = 48"),
            "live follow mode needs a small bottom tolerance for layout pixels"
        );
        assert!(
            js.contains("const follow = isNearBottom(el.detail)"),
            "the client must decide whether to follow before it changes the DOM"
        );
        assert!(
            js.contains("if (follow) el.detail.scrollTop = el.detail.scrollHeight"),
            "new deltas must reach the bottom only when the reader was already there"
        );
    }

    /// EXP-02's whole point, and the one thing a refactor loses in silence.
    /// The single-flavor call is the tempting one-liner: it writes `text/plain`
    /// alone, and the summary that was meant to arrive formatted in Slack
    /// arrives as a wall of markdown source instead. Both flavors must also ride
    /// in *one* `ClipboardItem` — the macOS pasteboard keeps only the first item
    /// of a multi-item write, so a second degrades to plain text on the only
    /// platform this ships on, with no error anywhere and only on a user's
    /// machine.
    #[test]
    fn a_copy_puts_both_flavors_on_one_clipboard_item() {
        let js = asset_text("app.js");
        assert!(
            js.contains("navigator.clipboard.write([item])"),
            "both flavors go in one ClipboardItem: macOS keeps only the first \
             item of a multi-item write, so a second is dropped in silence"
        );
        assert!(
            !js.contains("writeText"),
            "writeText carries no HTML flavor, and EXP-02 is about both"
        );
        for flavor in ["\"text/plain\"", "\"text/html\""] {
            assert!(
                js.contains(flavor),
                "EXP-02's {flavor} flavor must be written"
            );
        }
    }

    /// Safari treats the user gesture as spent at the first suspension point, so
    /// the write has to be reached with none — which is possible only because
    /// `currentDetail` already holds the meeting and the live pane keeps its own
    /// finals. The version that fetches its payload first works in Chrome, and
    /// Chrome is the trap: §10.1 says "it was blocked in Chrome" closes no
    /// ticket.
    ///
    /// Asserting the negative *and* the body, because a `contains("function
    /// copyNow(")` is satisfied by `async function copyNow(` — the exact
    /// refactor this is written to catch.
    #[test]
    fn the_copy_handler_reaches_the_clipboard_without_suspending() {
        let js = asset_text("app.js");
        assert!(
            !js.contains("async function copyNow"),
            "a suspension before the write loses the gesture, and Safari \
             refuses it"
        );
        let head = "function copyNow(payload) {";
        let start = js.find(head).expect("app.js must define copyNow");
        let rest = &js[start..];
        let body = &rest[..rest.find("\n}\n").expect("copyNow must be closed")];
        assert!(
            !body.contains("await"),
            "the copy handler must reach the clipboard with nothing suspended \
             before it, or the button works in Chrome and is dead in Safari: \
             {body}"
        );
    }

    /// ING-11 follows the words onto the clipboard: `text/html` is *live markup*
    /// in whatever application receives the paste, and a transcript is whatever
    /// anyone in the room said. The rich flavor is built as DOM nodes through
    /// the same `text()` helper the pane uses and handed to `XMLSerializer`,
    /// which escapes by construction — so no second escaper joins
    /// `fotw_store`'s `escape_html`. What a refactor reaches for instead is
    /// string concatenation, and `the_renderer_never_assigns_html` above would
    /// not notice.
    #[test]
    fn the_clipboards_html_is_serialized_rather_than_concatenated() {
        let js = asset_text("app.js");
        assert!(
            js.contains("new XMLSerializer().serializeToString"),
            "the HTML flavor must come from the serializer: a participant can \
             say anything, and a paste target parses what it is given"
        );
    }

    /// The reason this feature is client-side at all. The live pane's words are
    /// in no meeting row — the hub's deltas reach the store only after Stop — so
    /// copying a meeting *while it is recording* exists only here. And it reads
    /// a list kept beside the DOM rather than the rendered rows: the pane is
    /// trimmed to `MAX_ROWS` and holds a still-revising partial, so a copy taken
    /// off the screen would drop the top of a long meeting under a status line
    /// claiming the transcript.
    #[test]
    fn the_live_transcript_can_be_copied_before_the_meeting_is_saved() {
        let js = asset_text("app.js");
        assert!(
            js.contains("liveSegments.push("),
            "the live pane must keep its own finals, or its copy button has \
             nothing to read"
        );
        assert!(
            js.contains("transcriptBody(root, liveSegments)"),
            "the live copy reads the kept finals, not the trimmed rows on screen"
        );
        assert!(
            js.contains("\"live-copy\""),
            "the live copy row needs a marker `appendDeltas` can find, so it \
             appears only once there is a word to copy"
        );
    }

    /// A copy button is ordinary chrome. CON-02 reserves the indicator's red for
    /// one meaning and `--caution` is the caveat colour; a harmless local action
    /// must borrow neither. `.copy` is appended as the *last* selector of the
    /// shared button group because `css_rule` finds only a group's final
    /// selector — which is also why a future pin on `.gh-push` would need to
    /// give it its own rule. The row needs a gap: script-appended siblings carry
    /// no whitespace between them, so without one the buttons render as a single
    /// slab.
    #[test]
    fn the_copy_buttons_are_ordinary_chrome_in_a_spaced_row() {
        let css = asset_text("app.css");
        let rule = css_rule(&css, ".copy");
        assert!(
            rule.contains("var(--fg)") && rule.contains("var(--line)"),
            "a copy button looks like Save and Push: {rule}"
        );
        assert!(
            !rule.contains(INDICATOR_RED) && !rule.contains("var(--caution)"),
            "copying is neither a live capture nor a caution: {rule}"
        );
        let row = css_rule(&css, ".actions");
        assert!(
            row.contains("gap"),
            "without a gap the actions row renders as one slab of buttons: {row}"
        );
    }

    #[test]
    fn an_unknown_extension_is_not_served_as_html() {
        assert_eq!(content_type("x.bin"), "application/octet-stream");
        assert_eq!(content_type("x"), "application/octet-stream");
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
    }

    // ------------------------------------------------- FOTW_UI_DIR (#62)

    fn override_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fotw-uidir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The point of the feature: an allowlisted asset is read from the
    /// directory, so a UI edit needs no rebuild, no re-sign and no keychain
    /// prompt.
    #[test]
    fn an_embedded_name_is_read_from_the_override_directory() {
        let dir = override_dir("hit");
        std::fs::write(dir.join("app.js"), b"console.log(1);").unwrap();

        let got = dev_override_from(&dir, "app.js").expect("app.js is embedded");
        assert_eq!(got, b"console.log(1);");
    }

    /// The allowlist is the embedded bundle itself. A file that merely exists
    /// in the directory is not served — the override changes *where* known
    /// assets come from, never *what* the server exposes.
    #[test]
    fn a_name_the_bundle_does_not_contain_is_refused_even_if_the_file_exists() {
        let dir = override_dir("stranger");
        std::fs::write(dir.join("secrets.txt"), b"nope").unwrap();

        assert!(dev_override_from(&dir, "secrets.txt").is_none());
    }

    /// A traversal-shaped name is not in the embedded set, so it never reaches
    /// the filesystem — the join below the allowlist cannot be steered.
    #[test]
    fn a_traversal_name_never_reaches_the_filesystem() {
        let dir = override_dir("traverse");
        assert!(dev_override_from(&dir, "../Cargo.toml").is_none());
        assert!(dev_override_from(&dir, "..").is_none());
    }

    /// A missing file falls back to the embedded copy rather than 404ing the
    /// asset: half a directory is a normal state while editing.
    #[test]
    fn a_missing_file_means_the_embedded_copy_serves() {
        let dir = override_dir("fallback");
        assert!(dev_override_from(&dir, "app.js").is_none());
    }
}
