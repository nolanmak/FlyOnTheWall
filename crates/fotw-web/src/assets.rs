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
