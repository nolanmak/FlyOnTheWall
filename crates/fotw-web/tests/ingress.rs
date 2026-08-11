//! docs/REQUIREMENTS.md 10.1, end to end over a real socket.
//!
//! Each test below is named for the attack it refuses. The unit tests in
//! `src/ingress.rs` prove the predicates; these prove that the predicates are
//! actually wired into the server that listens — which is the half that
//! silently goes missing.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use common::{MEETING_ID, MEETING_TITLE, SEGMENT_TEXT};
use tower::ServiceExt as _;

// ------------------------------------------------------------ the happy path
//
// Stated first, because every refusal below is only meaningful if the same
// request minus the attack actually works. A suite where everything 404s
// passes every security assertion in it.

#[tokio::test]
async fn the_spa_and_the_api_work_for_the_real_client() {
    let h = common::start().await;

    let shell = h.get("/", &h.anonymous()).await;
    assert_eq!(shell.status, 200);
    assert!(shell.body.contains("FlyOnTheWall"));
    assert!(
        shell
            .header("content-security-policy")
            .is_some_and(|c| c.starts_with("default-src 'none'")),
        "ING-11: the shell must carry a strict CSP"
    );

    let meetings = h.get("/api/meetings", &h.authorised()).await;
    assert_eq!(meetings.status, 200);
    assert!(meetings.body.contains(MEETING_TITLE));

    let detail = h
        .get(&format!("/api/meetings/{MEETING_ID}"), &h.authorised())
        .await;
    assert_eq!(detail.status, 200);
    assert!(detail.body.contains(SEGMENT_TEXT));

    let hits = h.get("/api/search?q=loopback", &h.authorised()).await;
    assert_eq!(hits.status, 200);
    assert!(hits.body.contains(SEGMENT_TEXT));

    let ticket = h.post("/api/ws-ticket", &h.authorised(), None).await;
    assert_eq!(ticket.status, 200);
    assert!(ticket.body.contains("\"ticket\""));
}

// ------------------------------------------------------------------- ING-02
//
// The rebinding attack itself. The TCP connection really is to 127.0.0.1 —
// that is what rebinding achieves — and the browser sends the attacker's name
// in `Host` because script is not allowed to change it.

#[tokio::test]
async fn a_rebound_page_gets_a_404_even_holding_a_valid_token() {
    let h = common::start().await;
    for host in ["evil.test", "evil.test:80", "127.0.0.1.evil.test"] {
        let mut headers = h.authorised();
        headers[0] = ("Host".into(), host.into());
        let res = h.get("/api/meetings", &headers).await;
        assert_eq!(res.status, 404, "Host: {host} must be refused");
        assert!(res.body.is_empty());
        assert!(
            !res.body.contains(MEETING_TITLE),
            "not one byte of the library may leak"
        );
    }
}

/// ING-03. `axum_extra::extract::Host` prefers `Forwarded`, then
/// `X-Forwarded-Host`, then the real header — and a rebound page sets those
/// two freely, because after rebinding it is same-origin and there is no
/// forbidden-header list stopping it. A server built on that extractor accepts
/// every request below.
#[tokio::test]
async fn forwarded_headers_cannot_launder_a_rebound_host() {
    let h = common::start().await;
    for (name, value) in [
        ("Forwarded", format!("host={}", h.authority)),
        ("X-Forwarded-Host", h.authority.clone()),
        ("X-Forwarded-Server", h.authority.clone()),
        ("X-Original-Host", h.authority.clone()),
    ] {
        let mut headers = h.authorised();
        headers[0] = ("Host".into(), "evil.test".into());
        headers.push((name.into(), value));
        let res = h.get("/api/meetings", &headers).await;
        assert_eq!(res.status, 404, "{name} must not override Host");
    }
}

#[tokio::test]
async fn two_host_headers_are_refused() {
    let h = common::start().await;
    let mut headers = h.authorised();
    headers.push(("Host".into(), "evil.test".into()));
    assert_eq!(h.get("/api/meetings", &headers).await.status, 404);
}

#[tokio::test]
async fn another_port_on_this_machine_is_not_this_server() {
    let h = common::start().await;
    let mut headers = h.authorised();
    headers[0] = ("Host".into(), "127.0.0.1:1".into());
    assert_eq!(h.get("/api/meetings", &headers).await.status, 404);
}

// ------------------------------------------------------------------- ING-04

#[tokio::test]
async fn a_foreign_origin_is_refused_on_every_endpoint() {
    let h = common::start().await;
    for path in [
        "/",
        "/assets/app.js",
        "/api/meetings",
        &format!("/api/meetings/{MEETING_ID}"),
        "/api/search?q=loopback",
    ] {
        let mut headers = h.authorised();
        headers[1] = ("Origin".into(), "http://evil.test".into());
        let res = h.get(path, &headers).await;
        assert_eq!(res.status, 404, "{path} must refuse a foreign Origin");
    }
}

#[tokio::test]
async fn a_null_origin_is_not_a_missing_origin() {
    let h = common::start().await;
    let mut headers = h.authorised();
    headers[1] = ("Origin".into(), "null".into());
    assert_eq!(h.get("/api/meetings", &headers).await.status, 404);
}

/// A missing `Origin` is permitted — the CLI and `curl` send none, and no
/// browser attacker can suppress it.
#[tokio::test]
async fn a_missing_origin_is_permitted() {
    let h = common::start().await;
    let headers = vec![
        ("Host".into(), h.authority.clone()),
        ("Authorization".into(), format!("Bearer {}", h.token)),
    ];
    assert_eq!(h.get("/api/meetings", &headers).await.status, 200);
}

// ------------------------------------------------------------------- ING-05

#[tokio::test]
async fn no_token_no_data() {
    let h = common::start().await;
    for path in [
        "/api/meetings",
        &format!("/api/meetings/{MEETING_ID}"),
        "/api/search?q=loopback",
    ] {
        let res = h.get(path, &h.anonymous()).await;
        assert_eq!(res.status, 404, "{path} must require the bearer token");
        assert!(res.body.is_empty());
    }
    assert_eq!(
        h.post("/api/ws-ticket", &h.anonymous(), None).await.status,
        404
    );
}

#[tokio::test]
async fn a_token_from_another_daemon_start_is_worthless() {
    let a = common::start().await;
    let b = common::start().await;
    let mut headers = a.authorised();
    headers[2] = ("Authorization".into(), format!("Bearer {}", b.token));
    assert_eq!(a.get("/api/meetings", &headers).await.status, 404);
}

// ------------------------------------------------------------------- ING-08

/// Zero ambient credentials. A cookie set here would be sent to every other
/// service on `127.0.0.1` regardless of port (RFC 6265 scopes by host), and it
/// would make CSRF possible again.
#[tokio::test]
async fn nothing_this_server_returns_ever_sets_a_cookie() {
    let h = common::start().await;
    let launch = h.state.launch_url();
    let handoff = launch.split_once("?t=").unwrap().1.to_owned();

    let responses = vec![
        h.get("/", &h.anonymous()).await,
        h.get("/assets/app.js", &h.anonymous()).await,
        h.get("/api/meetings", &h.authorised()).await,
        h.get("/api/search?q=loopback", &h.authorised()).await,
        h.post("/api/ws-ticket", &h.authorised(), None).await,
        h.post(
            "/api/handoff",
            &h.anonymous(),
            Some(&format!("{{\"token\":\"{handoff}\"}}")),
        )
        .await,
        h.get("/api/meetings", &h.anonymous()).await,
    ];
    for res in responses {
        assert!(
            res.header("set-cookie").is_none(),
            "ING-08: no cookies, ever — got {:?}",
            res.headers
        );
    }
}

// ------------------------------------------------------------------- ING-09

/// The claim, tested as bytes: a caller cannot tell "your token is wrong" from
/// "there is nothing here".
#[tokio::test]
async fn a_bad_token_and_an_unknown_path_are_byte_identical() {
    let h = common::start().await;

    let mut bad_token = h.authorised();
    bad_token[2] = ("Authorization".into(), format!("Bearer {}", "0".repeat(64)));
    let refused = h.get("/api/meetings", &bad_token).await;
    let unknown = h.get("/no-such-path", &h.authorised()).await;

    assert_eq!(refused.status, 404);
    assert_eq!(unknown.status, 404);
    assert_eq!(
        refused.bytes_without_date(),
        unknown.bytes_without_date(),
        "ING-09: the two responses must be indistinguishable\nrefused: {}\nunknown: {}",
        String::from_utf8_lossy(&refused.raw),
        String::from_utf8_lossy(&unknown.raw),
    );
}

/// Every other refusal has to be the same response too, not just those two.
#[tokio::test]
async fn every_refusal_is_the_same_response() {
    let h = common::start().await;
    let baseline = h.get("/no-such-path", &h.authorised()).await;

    let mut bad_host = h.authorised();
    bad_host[0] = ("Host".into(), "evil.test".into());
    let mut bad_origin = h.authorised();
    bad_origin[1] = ("Origin".into(), "http://evil.test".into());
    let mut bad_token = h.authorised();
    bad_token[2] = ("Authorization".into(), "Bearer nope".into());

    let refusals = vec![
        ("host", h.get("/api/meetings", &bad_host).await),
        ("origin", h.get("/api/meetings", &bad_origin).await),
        ("token", h.get("/api/meetings", &bad_token).await),
        ("no token", h.get("/api/meetings", &h.anonymous()).await),
        (
            "unknown meeting",
            h.get("/api/meetings/does-not-exist", &h.authorised()).await,
        ),
        (
            "unknown asset",
            h.get("/assets/nope.js", &h.anonymous()).await,
        ),
        ("no ticket", h.get("/api/stream", &h.authorised()).await),
        (
            "bad handoff",
            h.post("/api/handoff", &h.anonymous(), Some("{\"token\":\"x\"}"))
                .await,
        ),
    ];

    for (what, res) in refusals {
        assert_eq!(
            res.bytes_without_date(),
            baseline.bytes_without_date(),
            "the {what} refusal is distinguishable from a plain 404: {}",
            String::from_utf8_lossy(&res.raw)
        );
    }
}

/// A `405 Method Not Allowed` with an `Allow` header would tell an
/// unauthenticated caller that a path exists and what it accepts.
#[tokio::test]
async fn a_wrong_method_does_not_confirm_that_a_path_exists() {
    let h = common::start().await;
    let baseline = h.get("/no-such-path", &h.authorised()).await;
    let res = h.post("/api/meetings", &h.authorised(), Some("{}")).await;
    assert_eq!(res.status, 404);
    assert!(res.header("allow").is_none());
    assert_eq!(res.bytes_without_date(), baseline.bytes_without_date());
}

// ------------------------------------------------------------------- ING-10

#[tokio::test]
async fn the_launch_token_buys_the_bearer_exactly_once() {
    let h = common::start().await;
    let launch = h.state.launch_url();
    assert!(launch.starts_with(&format!("{}/?t=", h.origin)));
    assert!(
        !launch.contains(&h.token),
        "the session secret must never be in a URL — `open(1)` puts it in argv"
    );
    let handoff = launch.split_once("?t=").unwrap().1.to_owned();
    let body = format!("{{\"token\":\"{handoff}\"}}");

    let first = h.post("/api/handoff", &h.anonymous(), Some(&body)).await;
    assert_eq!(first.status, 200);
    assert!(
        first.body.contains(&h.token),
        "redeeming the handoff must yield the bearer token"
    );

    let replay = h.post("/api/handoff", &h.anonymous(), Some(&body)).await;
    assert_eq!(
        replay.status, 404,
        "the handoff token is burned on redemption"
    );
}

#[tokio::test]
async fn a_rebound_page_cannot_redeem_a_handoff_token() {
    let h = common::start().await;
    let handoff = h.state.launch_url().split_once("?t=").unwrap().1.to_owned();
    let body = format!("{{\"token\":\"{handoff}\"}}");

    let mut headers = h.anonymous();
    headers[0] = ("Host".into(), "evil.test".into());
    assert_eq!(
        h.post("/api/handoff", &headers, Some(&body)).await.status,
        404
    );

    // And the token survives, so the real page can still use it.
    let ok = h.post("/api/handoff", &h.anonymous(), Some(&body)).await;
    assert_eq!(ok.status, 200);
}

#[tokio::test]
async fn the_shell_is_served_with_no_referrer() {
    let h = common::start().await;
    let shell = h.get("/", &h.anonymous()).await;
    assert_eq!(shell.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(shell.header("x-content-type-options"), Some("nosniff"));
    assert!(
        shell
            .header("cache-control")
            .is_some_and(|c| c.contains("no-store")),
        "transcripts must not land in the browser's disk cache"
    );
}

// ------------------------------------------------------------------- ING-01
//
// The peer tripwire cannot be exercised over loopback — every peer there is
// loopback, which is the point. So the router is driven directly with the
// `ConnectInfo` a non-loopback connection would have carried.

#[tokio::test]
async fn a_non_loopback_peer_is_refused_even_with_a_perfect_request() {
    let h = common::start().await;
    let app = fotw_web::router(h.state.clone());

    let request = |peer: IpAddr| {
        let mut req = Request::builder()
            .uri("/api/meetings")
            .header("host", &h.authority)
            .header("origin", &h.origin)
            .header("authorization", format!("Bearer {}", h.token))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer, 4444)));
        req
    };

    let lan = app
        .clone()
        .oneshot(request(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 44))))
        .await
        .unwrap();
    assert_eq!(
        lan.status(),
        StatusCode::NOT_FOUND,
        "ING-01: a LAN peer must get nothing, whatever else it got right"
    );

    let local = app
        .oneshot(request(IpAddr::V4(Ipv4Addr::LOCALHOST)))
        .await
        .unwrap();
    assert_eq!(
        local.status(),
        StatusCode::OK,
        "the identical request from loopback must succeed — otherwise the \
         assertion above proves nothing"
    );
}

// --------------------------------------------------------------- API shape

#[tokio::test]
async fn an_unknown_meeting_is_indistinguishable_from_an_unauthorised_one() {
    let h = common::start().await;
    let unknown = h
        .get(
            "/api/meetings/01234567-89ab-7000-8000-000000000000",
            &h.authorised(),
        )
        .await;
    let unauthorised = h
        .get(&format!("/api/meetings/{MEETING_ID}"), &h.anonymous())
        .await;
    assert_eq!(unknown.status, 404);
    assert_eq!(
        unknown.bytes_without_date(),
        unauthorised.bytes_without_date()
    );
}

#[tokio::test]
async fn a_search_with_no_searchable_token_is_an_empty_list_not_an_error() {
    let h = common::start().await;
    for query in ["", "%20", "q=", "+++"] {
        let res = h
            .get(&format!("/api/search?q={query}"), &h.authorised())
            .await;
        assert_eq!(res.status, 200, "?q={query} should not be an error");
        assert!(res.body.contains("\"hits\":[]"), "got {}", res.body);
    }
}
