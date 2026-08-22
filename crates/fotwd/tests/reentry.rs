//! Re-entry: a second `serve` against a live daemon opens a new authorized
//! tab instead of starting a second daemon or telling the user to go find a
//! menu bar that is not there yet (#58).
//!
//! The launch URL is one-time by design, so a closed tab used to be a dead
//! end. The daemon now answers `POST /api/launch-url` to a caller holding
//! the bearer — which the CLI proves it is by reading the 0600 state file —
//! and hands back a fresh one-time link.

use std::sync::Arc;

use fotw_web::{MemorySource, WebServer};
use fotwd::serve::fetch_fresh_launch_url;

#[tokio::test]
async fn a_live_daemon_hands_a_second_serve_a_working_login_link() {
    let server = WebServer::bind(0, Arc::new(MemorySource::new()))
        .await
        .expect("bind loopback");
    let port = server.addr().port();
    let state = server.state().clone();
    let token = state.policy().secret().expose_hex();
    tokio::spawn(async move {
        let _ = server.serve().await;
    });

    let url = fetch_fresh_launch_url(port, &token)
        .await
        .expect("a live daemon mints a link");
    assert!(url.starts_with(&format!("http://127.0.0.1:{port}/?t=")));
    assert!(
        !url.contains(&token),
        "the bearer must never appear in a URL"
    );

    // The link actually logs a tab in, exactly once.
    let handoff = url.split_once("?t=").unwrap().1.to_owned();
    let client = reqwest::Client::new();
    let redeem = || {
        client
            .post(format!("http://127.0.0.1:{port}/api/handoff"))
            .json(&serde_json::json!({ "token": handoff }))
            .send()
    };
    let first = redeem().await.expect("loopback reachable");
    assert_eq!(first.status().as_u16(), 200);
    let second = redeem().await.expect("loopback reachable");
    assert_eq!(second.status().as_u16(), 404, "burned on redemption");
}

#[tokio::test]
async fn a_dead_or_foreign_daemon_is_not_mistaken_for_a_live_one() {
    // Nothing listens here: the caller must fall through to a fresh start.
    let vacant = {
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        sock.local_addr().unwrap().port()
        // Dropped: the port is free again.
    };
    assert!(fetch_fresh_launch_url(vacant, "deadbeef").await.is_err());

    // Something else entirely listens: a wrong token answers 404, and 404 is
    // "not our daemon", never "open that".
    let server = WebServer::bind(0, Arc::new(MemorySource::new()))
        .await
        .expect("bind loopback");
    let port = server.addr().port();
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    assert!(
        fetch_fresh_launch_url(port, "0000000000000000")
            .await
            .is_err(),
        "a stale state file naming a reused port must not hijack the launch"
    );
}
