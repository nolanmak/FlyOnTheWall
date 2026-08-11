//! Identity and time, the two things every row in the schema carries.

use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

/// A fresh UUIDv7 (RFC 9562) rendered as lowercase hyphenated text.
///
/// v7 puts a 48-bit millisecond timestamp in the high bits, so ids sort in
/// creation order. That is what lets every primary key in the schema be an
/// opaque `TEXT` — inserts still land at the right edge of the B-tree the way
/// an autoincrement would, without the autoincrement's fatal property for
/// sync: two devices generating "the next integer" collide, two devices
/// generating v7s do not (docs/REQUIREMENTS.md 9.7 invariant 1).
#[must_use]
pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}

/// Milliseconds since the Unix epoch, UTC.
///
/// Every timestamp column in the schema is this. Display timezone is stored
/// separately as an IANA name on `meetings.tz`, so a meeting recorded in
/// Berlin still renders in Berlin time after the laptop moves (§9.3).
///
/// Saturates at 0 if the system clock is set before 1970 rather than panicking
/// — a nonsense clock must not be able to take the recorder down mid-meeting.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_v7_and_sort_in_creation_order() {
        let mut ids: Vec<String> = (0..64).map(|_| new_id()).collect();
        let sorted = {
            let mut c = ids.clone();
            c.sort();
            c
        };
        assert_eq!(ids, sorted, "UUIDv7 must be lexicographically time-ordered");

        ids.dedup();
        assert_eq!(ids.len(), 64, "ids must be unique");

        let parsed = Uuid::parse_str(&ids[0]).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
    }

    #[test]
    fn now_ms_is_milliseconds_not_seconds() {
        // 2020-01-01 in ms. If anyone ever "fixes" this to seconds, the
        // magnitude check catches it before the schema silently mixes units.
        assert!(now_ms() > 1_577_836_800_000);
    }
}
