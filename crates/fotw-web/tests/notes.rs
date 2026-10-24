#![cfg(feature = "store")]
mod common;
use fotw_store::{Db, DbKey, NewMeeting};
use fotw_web::{StoreSource, WebServer};

#[tokio::test]
async fn notes_require_auth_and_stale_writes_cannot_replace_saved_text() {
    let mut db = Db::open_in_memory(&DbKey::from_bytes([1; 32])).unwrap();
    let id = db
        .meetings()
        .create(NewMeeting::new("fixture", "UTC"))
        .unwrap();
    let server = WebServer::bind(0, std::sync::Arc::new(StoreSource::new(db)))
        .await
        .unwrap();
    let state = server.state().clone();
    let h = common::Harness {
        addr: server.addr(),
        token: state.policy().secret().expose_hex(),
        authority: state.policy().authority().to_owned(),
        origin: state.policy().origin().to_owned(),
        state,
    };
    let task = tokio::spawn(server.serve());
    let path = format!("/api/meetings/{id}/notes");
    assert_eq!(h.get(&path, &h.anonymous()).await.status, 404);
    let initial = h.get(&path, &h.authorised()).await;
    assert_eq!(initial.status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&initial.body).unwrap()["revision"],
        0
    );
    let body =
        r#"{"revision":0,"body_md":"Decision: café <script>alert(1)</script>","anchors":[]}"#;
    assert_eq!(h.post(&path, &h.anonymous(), Some(body)).await.status, 404);
    let saved = h.post(&path, &h.authorised(), Some(body)).await;
    assert_eq!(saved.status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&saved.body).unwrap()["revision"],
        1
    );
    let conflict = h
        .post(
            &path,
            &h.authorised(),
            Some(r#"{"revision":0,"body_md":"stale","anchors":[]}"#),
        )
        .await;
    assert_eq!(conflict.status, 409);
    assert!(h.get(&path, &h.authorised()).await.body.contains("café"));
    assert!(
        h.get(&format!("/api/meetings/{id}"), &h.authorised())
            .await
            .body
            .contains("café")
    );
    assert!(
        h.get("/api/search?q=café", &h.authorised())
            .await
            .body
            .contains(&id)
    );
    assert_eq!(
        h.get("/api/meetings/missing/notes", &h.authorised())
            .await
            .status,
        404
    );
    let invalid = h.post(&path, &h.authorised(), Some(r#"{"revision":1,"body_md":"invalid","anchors":[{"block_idx":0,"block_text":"bad","typed_at_ms":-1}]}"#)).await;
    assert_eq!(invalid.status, 422);
    let mut hostile = h.authorised();
    hostile
        .iter_mut()
        .find(|(key, _)| key == "Origin")
        .unwrap()
        .1 = "https://other.example".into();
    assert_eq!(
        h.post(
            &path,
            &hostile,
            Some(r#"{"revision":1,"body_md":"hostile","anchors":[]}"#)
        )
        .await
        .status,
        404
    );
    let after = h.get(&path, &h.authorised()).await;
    let after: serde_json::Value = serde_json::from_str(&after.body).unwrap();
    assert_eq!(after["revision"], 1);
    assert!(after["body_md"].as_str().unwrap().contains("café"));
    task.abort();
}
