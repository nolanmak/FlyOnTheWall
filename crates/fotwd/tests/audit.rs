//! The audit log CON-01's acceptance criterion is stated against.

use fotwd::audit::{AuditKind, AuditLog};

fn tmpdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fotw-audit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sessions")).expect("temp dir");
    dir.join("sessions")
}

#[test]
fn a_start_is_recorded_with_who_asked_and_what_they_were_told() {
    let root = tmpdir("start");
    let log = AuditLog::at(&root);

    log.record(AuditKind::SessionStart {
        origin: "detection-prompt".to_owned(),
        detected_app: Some("us.zoom.xos".to_owned()),
        jurisdiction_warning: "California requires every participant's consent".to_owned(),
        acknowledged_all_party: true,
    })
    .expect("write");

    let events = log.read().expect("read");
    assert_eq!(events.len(), 1);
    let AuditKind::SessionStart {
        origin,
        detected_app,
        acknowledged_all_party,
        ..
    } = &events[0].kind
    else {
        panic!("wrong event: {:?}", events[0]);
    };
    assert_eq!(origin, "detection-prompt");
    assert_eq!(detected_app.as_deref(), Some("us.zoom.xos"));
    assert!(acknowledged_all_party);
    assert!(events[0].at_unix_ms > 1_600_000_000_000, "wall-clock stamp");
}

#[test]
fn the_log_appends_and_never_rewrites() {
    // A rewritten file loses everything on a crash mid-write. This is the
    // record that answers "did anyone consent"; losing it is not an option.
    let root = tmpdir("append");
    let log = AuditLog::at(&root);
    for i in 0..5u64 {
        log.record_at(
            1_700_000_000_000 + i,
            AuditKind::SessionEnd {
                session: format!("s{i}"),
                duration_ms: i * 1_000,
            },
        )
        .expect("write");
    }

    let events = log.read().expect("read");
    assert_eq!(events.len(), 5);
    assert!(
        events.windows(2).all(|w| w[0].at_unix_ms < w[1].at_unix_ms),
        "events must stay in the order they happened"
    );
}

#[test]
fn a_torn_last_line_does_not_make_the_log_unreadable() {
    let root = tmpdir("torn");
    let log = AuditLog::at(&root);
    log.record(AuditKind::DetectionDeclined {
        app: "us.zoom.xos".to_owned(),
        answer: "not_now".to_owned(),
    })
    .expect("write");

    // Simulate `kill -9` between the write and the newline.
    let mut raw = std::fs::read_to_string(log.path()).expect("read");
    raw.push_str("{\"at_unix_ms\":17000000");
    std::fs::write(log.path(), raw).expect("write");

    let events = log.read().expect("a damaged tail must not fail the read");
    assert_eq!(events.len(), 1, "the intact records must survive");
}

#[test]
fn reading_a_log_that_does_not_exist_yet_is_not_an_error() {
    let root = tmpdir("missing");
    assert!(AuditLog::at(&root).read().expect("empty read").is_empty());
}
