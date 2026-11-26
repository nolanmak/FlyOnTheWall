//! Release settings (#326 5.1, D2, D17).
//!
//! Stored as JSON in `app_meta` under [`SETTINGS_KEY`], so they never sync
//! and never ride in a library archive. A missing key means the defaults
//! (release off). A key that exists but cannot be read, decoded or validated
//! is **damaged**: the worker pauses with `settings_unreadable` and nothing
//! falls back to defaults, because a damaged value must not fail open.

use fotw_store::Db;
use fotw_web::release::ReleaseSettings;
use rusqlite::OptionalExtension as _;

/// The `app_meta` key holding [`ReleaseSettings`].
pub const SETTINGS_KEY: &str = "release_settings";

/// `app_meta`: when the owner notice last went out (D16).
pub const NOTICE_KEY: &str = "release_notice_last_ms";

/// `app_meta`: when a row last entered `needs_review` or `held` (D16).
pub const ATTENTION_KEY: &str = "release_attention_last_ms";

/// The largest `send_delay_minutes` (D11).
pub const MAX_SEND_DELAY_MINUTES: u32 = 120;

/// What is stored under [`SETTINGS_KEY`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// Never saved: the defaults, release off.
    Missing,
    /// A complete, valid value.
    Valid(ReleaseSettings),
    /// Present but unreadable, undecodable or invalid (`settings_unreadable`).
    Damaged,
}

impl Stored {
    /// The settings in force, or `None` when they are damaged.
    #[must_use]
    pub fn in_force(self) -> Option<ReleaseSettings> {
        match self {
            Self::Missing => Some(ReleaseSettings::default()),
            Self::Valid(s) => Some(s),
            Self::Damaged => None,
        }
    }
}

/// Read the stored settings.
#[must_use]
pub fn load(db: &Db) -> Stored {
    let raw: Result<Option<String>, _> = db
        .conn()
        .query_row(
            "SELECT value FROM app_meta WHERE key = ?1",
            [SETTINGS_KEY],
            |r| r.get(0),
        )
        .optional();
    match raw {
        Ok(None) => Stored::Missing,
        Ok(Some(text)) => match serde_json::from_str::<ReleaseSettings>(&text) {
            Ok(settings) if validate(&settings).is_ok() => Stored::Valid(settings),
            _ => Stored::Damaged,
        },
        Err(_) => Stored::Damaged,
    }
}

/// Store `settings` as given. Validation is the caller's job.
///
/// # Errors
///
/// The write failed.
pub fn write(db: &Db, settings: &ReleaseSettings, now_ms: i64) -> rusqlite::Result<()> {
    let json = serde_json::to_string(settings).unwrap_or_else(|_| "{}".to_owned());
    put_meta(db.conn(), SETTINGS_KEY, &json, now_ms)
}

/// Upsert one `app_meta` row.
pub(crate) fn put_meta(
    conn: &rusqlite::Connection,
    key: &str,
    value: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO app_meta (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        rusqlite::params![key, value, now_ms],
    )?;
    Ok(())
}

/// Read one `app_meta` row as an integer.
pub(crate) fn get_meta_ms(conn: &rusqlite::Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM app_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(value.and_then(|v| v.trim().parse().ok()))
}

/// Whether `domain` is a valid `allowed_domains` entry: lowercase ASCII, no
/// `@`, no `*`, at least one dot, no empty label (5.1).
#[must_use]
pub fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.contains('.')
        && domain
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && domain.split('.').all(|label| !label.is_empty())
}

/// Check every field rule of 5.1. Errors are user-facing and content-free.
///
/// # Errors
///
/// The first rule `settings` breaks.
pub fn validate(settings: &ReleaseSettings) -> Result<(), &'static str> {
    if !settings.allowed_domains.iter().all(|d| valid_domain(d)) {
        return Err(
            "Each allowed domain must be lowercase ASCII with at least one dot, and no @ or *.",
        );
    }
    if settings.send_delay_minutes > MAX_SEND_DELAY_MINUTES {
        return Err("The send delay must be between 0 and 120 minutes.");
    }
    let notify = settings.notify_address.trim();
    if !notify.is_empty()
        && !crate::calendar::release::normalize_address(notify)
            .is_some_and(|a| a == notify && super::address_allowed(&a, &settings.allowed_domains))
    {
        return Err("The notice address must be a lowercase address in an allowed domain.");
    }
    if settings.enabled && settings.allowed_domains.is_empty() {
        return Err("Add at least one allowed domain before turning release on.");
    }
    Ok(())
}

/// Apply the server-owned fields (D2): `release_since_ms` is stamped `now`
/// on every switch from disabled to enabled and otherwise kept; the client's
/// value is ignored. `verified_sender` is the stored one; only a passing
/// preflight changes it.
#[must_use]
pub fn stamp(
    previous: Option<&ReleaseSettings>,
    mut incoming: ReleaseSettings,
    now_ms: i64,
) -> ReleaseSettings {
    let was_enabled = previous.is_some_and(|p| p.enabled);
    incoming.release_since_ms = if incoming.enabled && !was_enabled {
        now_ms
    } else {
        previous.map_or(0, |p| p.release_since_ms)
    };
    incoming.verified_sender = previous
        .map(|p| p.verified_sender.clone())
        .unwrap_or_default();
    incoming
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_web::release::ReleaseMode;

    fn db() -> Db {
        Db::open_in_memory(&fotw_store::DbKey::from_bytes([3; 32])).unwrap()
    }

    fn on() -> ReleaseSettings {
        ReleaseSettings {
            enabled: true,
            allowed_domains: vec!["example.com".into()],
            ..ReleaseSettings::default()
        }
    }

    #[test]
    fn a_missing_key_is_the_defaults_and_release_is_off() {
        let db = db();
        assert_eq!(load(&db), Stored::Missing);
        let s = load(&db).in_force().unwrap();
        assert!(!s.enabled);
        assert_eq!(s.mode, ReleaseMode::Approval);
        assert_eq!(s.send_delay_minutes, 15);
    }

    #[test]
    fn a_damaged_value_is_never_the_defaults() {
        let db = db();
        for bad in [
            "not json",
            r#"{"enabled":"yes"}"#,
            r#"{"enabled":true,"allowed_domains":["Example.COM"]}"#,
            r#"{"send_delay_minutes":121}"#,
            r#"{"mode":"sometimes"}"#,
        ] {
            put_meta(db.conn(), SETTINGS_KEY, bad, 1).unwrap();
            assert_eq!(load(&db), Stored::Damaged, "{bad}");
            assert_eq!(load(&db).in_force(), None);
        }
        put_meta(db.conn(), SETTINGS_KEY, r#"{"enabled":false}"#, 1).unwrap();
        assert!(
            matches!(load(&db), Stored::Valid(_)),
            "missing fields default"
        );
    }

    #[test]
    fn domains_are_validated() {
        for good in ["example.com", "a-b.example.org", "xn--bcher-kva.example"] {
            assert!(valid_domain(good), "{good}");
        }
        for bad in [
            "",
            "example",
            "Example.com",
            "*.example.com",
            "a@example.com",
            "example..com",
            ".example.com",
            "exämple.com",
        ] {
            assert!(!valid_domain(bad), "{bad}");
        }
        let mut s = on();
        s.allowed_domains.push("*.example.org".into());
        assert!(validate(&s).is_err());
    }

    #[test]
    fn delay_and_notify_rules() {
        let mut s = on();
        s.send_delay_minutes = 0;
        assert!(validate(&s).is_ok());
        s.send_delay_minutes = 120;
        assert!(validate(&s).is_ok());
        s.send_delay_minutes = 121;
        assert!(validate(&s).is_err());
        s.send_delay_minutes = 15;
        s.notify_address = "owner@example.org".into();
        assert!(validate(&s).is_err(), "notice outside the allowed domains");
        s.notify_address = "owner@example.com".into();
        assert!(validate(&s).is_ok());
        let mut off = on();
        off.allowed_domains.clear();
        assert!(validate(&off).is_err(), "enabling needs a domain");
        off.enabled = false;
        assert!(validate(&off).is_ok());
    }

    #[test]
    fn the_cutoff_is_restamped_on_enable_and_a_client_stamp_is_ignored() {
        let mut incoming = on();
        incoming.release_since_ms = 42;
        incoming.verified_sender = "attacker@example.com".into();
        let first = stamp(None, incoming.clone(), 1_000);
        assert_eq!(first.release_since_ms, 1_000);
        assert_eq!(first.verified_sender, "");

        // Saving again while enabled keeps the stamp.
        let again = stamp(Some(&first), incoming.clone(), 2_000);
        assert_eq!(again.release_since_ms, 1_000);

        // Off, then on again: a new cutoff.
        let mut disabled = incoming.clone();
        disabled.enabled = false;
        let off = stamp(Some(&again), disabled, 3_000);
        assert_eq!(off.release_since_ms, 1_000);
        let back = stamp(Some(&off), incoming, 4_000);
        assert_eq!(back.release_since_ms, 4_000);
    }
}
