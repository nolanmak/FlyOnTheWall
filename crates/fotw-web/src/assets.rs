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
    let Some(file) = Ui::get(path) else {
        return not_found();
    };
    let body = Body::from(file.data.into_owned());
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .expect("a static asset response is always constructible");
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type(path));
    apply_security_headers(headers, state.csp());
    response
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

    #[test]
    fn an_unknown_extension_is_not_served_as_html() {
        assert_eq!(content_type("x.bin"), "application/octet-stream");
        assert_eq!(content_type("x"), "application/octet-stream");
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
    }
}
