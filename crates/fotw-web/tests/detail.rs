//! What the meeting-detail response actually carries.
//!
//! This file exists because the suite had 87 passing tests while the API was
//! silently dropping every transcript field except `idx` and `text`, and never
//! returning the user's notes at all. Nothing failed, because nothing asserted
//! on the *shape* of the payload — the ingress tests check status codes and
//! headers, and the stream tests check deltas. The gap was only visible by
//! opening the page in a browser and noticing the transcript had no speakers.
//!
//! So these assert on fields by name. A test that only checked "the response
//! contains the transcript text" would have passed throughout the whole bug.

mod common;

use common::{MEETING_ID, NOTE_TEXT, SEGMENT_TEXT};

fn detail_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("detail response is JSON")
}

#[tokio::test]
async fn a_segment_carries_its_speaker_and_its_offset() {
    let h = common::start().await;
    let res = h
        .get(&format!("/api/meetings/{MEETING_ID}"), &h.authorised())
        .await;
    assert_eq!(res.status, 200);

    let body = detail_json(&res.body);
    let seg = &body["segments"][0];

    assert_eq!(
        seg["text"], SEGMENT_TEXT,
        "the words themselves must survive"
    );
    // The two that were being dropped. Diarisation is requested from the
    // provider and paid for; a transcript that renders as an anonymous
    // monologue throws that away, and an offset is how a reader finds the
    // moment in the audio.
    assert_eq!(
        seg["speaker"], "S0",
        "the speaker label was stored but never returned"
    );
    assert_eq!(
        seg["start_ms"], 12_000,
        "the offset was stored but never returned"
    );
}

#[tokio::test]
async fn the_users_own_notes_come_back_with_the_meeting() {
    let h = common::start().await;
    let res = h
        .get(&format!("/api/meetings/{MEETING_ID}"), &h.authorised())
        .await;

    let body = detail_json(&res.body);
    // Search indexes notes, so before this a user could match their own note,
    // click the result, and find the note nowhere on the page.
    assert_eq!(body["note_md"], NOTE_TEXT);
}

/// The note and the transcript are different things and must not be conflated.
///
/// Asserting only "the response contains the note text" would also pass if the
/// note were accidentally rendered into the transcript array, which is exactly
/// the kind of near-miss that makes a suite look healthier than it is.
#[tokio::test]
async fn the_note_is_not_smuggled_in_as_a_transcript_segment() {
    let h = common::start().await;
    let res = h
        .get(&format!("/api/meetings/{MEETING_ID}"), &h.authorised())
        .await;

    let body = detail_json(&res.body);
    let segments = body["segments"].as_array().expect("segments is an array");
    assert_eq!(segments.len(), 1, "the note must not become a segment");
    for seg in segments {
        assert_ne!(seg["text"], NOTE_TEXT);
    }
}

/// A meeting with no notes must say so with `null`, not with `""`.
///
/// The renderer branches on truthiness, so an empty string and a missing note
/// behave the same today — but a "Your notes" heading over an empty box is a
/// different claim than no notes section at all, and the API should not make
/// the client guess which happened.
#[tokio::test]
async fn a_meeting_without_notes_returns_null_not_empty_string() {
    use fotw_web::{MeetingDetail, MeetingRow, MemorySource, WebServer};

    let source = MemorySource::new().with_meeting(MeetingDetail {
        meeting: MeetingRow {
            id: MEETING_ID.to_owned(),
            title: "No notes".to_owned(),
            started_at_ms: 1_754_900_000_000,
            duration_ms: Some(60_000),
            state: "ready".to_owned(),
        },
        summary_md: None,
        note_md: None,
        segments: Vec::new(),
    });
    let server = WebServer::bind(0, std::sync::Arc::new(source))
        .await
        .expect("bind");
    let addr = server.addr();
    let state = server.state().clone();
    let headers = vec![
        ("Host".to_string(), state.policy().authority().to_owned()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", state.policy().secret().expose_hex()),
        ),
    ];
    tokio::spawn(async move {
        let _ = server.serve().await;
    });

    let res = common::send(
        addr,
        &common::build(
            "GET",
            &format!("/api/meetings/{MEETING_ID}"),
            &headers,
            None,
        ),
    )
    .await;

    let body = detail_json(&res.body);
    assert!(
        body["note_md"].is_null(),
        "expected null, got {}",
        body["note_md"]
    );
}
