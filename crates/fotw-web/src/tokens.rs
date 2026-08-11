//! Single-use, short-lived tokens — ING-07 (WS tickets) and ING-10 (launch
//! handoff).
//!
//! Both controls are the same object with a different clock, so they are one
//! type.
//!
//! # ING-07, and why a cookie is not an option
//!
//! `Authorization: Bearer` cannot authenticate a WebSocket from a browser.
//! The whole client API is `new WebSocket(url, protocols)` — there is no
//! header argument and there never has been. The usual workaround is a
//! session cookie, because cookies ride along automatically. That workaround
//! is forbidden here by **ING-08**: RFC 6265 scopes cookies by *host*, not by
//! origin, so a cookie set by `127.0.0.1:51234` is attached to requests to
//! `127.0.0.1` on **every other port** — every other local dev server, every
//! other Electron app's helper, anything the user happens to be running. It
//! also re-introduces ambient authority, which is the ingredient CSRF needs.
//!
//! So the credential travels in the URL instead, and everything about this
//! type is a consequence of that being a leaky place to put a secret: the
//! ticket is worth one connection, it dies in ten seconds, and it can only be
//! minted by a caller that already holds the bearer token.
//!
//! # ING-10, and why the handoff token dies even faster than it looks
//!
//! The daemon opens the UI with `open(1)`, which puts the URL in the **process
//! argv** — readable by every process of the same user via `ps` — and in the
//! browser's history, which on macOS syncs to iCloud and thence to every other
//! device on the account. A long-lived token there is a long-lived leak, so
//! the launch URL carries a token that is worth exactly one redemption inside
//! thirty seconds, and the SPA strips it from the address bar with
//! `history.replaceState` as its first act.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::secret::{random_token, tokens_match};

/// ING-07: a WS ticket is worth ten seconds.
///
/// The legitimate gap between "POST /api/ws-ticket returned" and "the upgrade
/// request arrived" is a millisecond or two on loopback. Ten seconds is
/// already three orders of magnitude of slack for a wedged laptop.
pub const WS_TICKET_TTL: Duration = Duration::from_secs(10);

/// ING-10: the launch handoff is worth thirty seconds.
///
/// Long enough for a cold browser start on a slow machine, short enough that
/// the copy in `ps` output and in synced history is inert by the time anyone
/// could read it.
pub const HANDOFF_TTL: Duration = Duration::from_secs(30);

/// How many unredeemed tokens may be outstanding.
///
/// Minting requires the bearer token, so this is not an anti-DoS measure; it
/// is a bound on how long a mistake can accumulate. When the table is full the
/// oldest entry is dropped, which is safe because the oldest entry is also the
/// closest to expiring.
const CAPACITY: usize = 64;

struct Entry {
    token: String,
    minted: Instant,
}

/// A table of one-time tokens with a fixed lifetime.
#[derive(Debug)]
pub struct TokenTable {
    ttl: Duration,
    entries: Mutex<Vec<Entry>>,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // §10's never-log rule: a live ticket is a credential.
        f.write_str("Entry(<redacted>)")
    }
}

impl TokenTable {
    /// An empty table whose tokens live for `ttl`.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Mint a token and remember it until it is redeemed or expires.
    #[must_use]
    pub fn mint(&self) -> String {
        let token = random_token();
        let mut entries = self.lock();
        Self::prune(&mut entries, self.ttl);
        if entries.len() >= CAPACITY {
            entries.remove(0);
        }
        entries.push(Entry {
            token: token.clone(),
            minted: Instant::now(),
        });
        token
    }

    /// Spend `presented`. True at most once per minted token, and never after
    /// its TTL.
    ///
    /// The scan is a constant-time comparison per entry rather than a hash
    /// lookup. A `HashMap` would compare with `==` on `String`, which returns
    /// at the first differing byte, and a caller who can measure that can walk
    /// a live ticket out of the server one character at a time — inside its
    /// ten-second window, on loopback, where the timing signal is at its
    /// clearest.
    #[must_use]
    pub fn redeem(&self, presented: &str) -> bool {
        let mut entries = self.lock();
        Self::prune(&mut entries, self.ttl);
        let Some(idx) = entries
            .iter()
            .position(|e| tokens_match(&e.token, presented))
        else {
            return false;
        };
        // Burned on redemption whether or not the caller goes on to succeed:
        // a ticket buys one attempt, not one success.
        entries.remove(idx);
        true
    }

    /// How many tokens are outstanding. Tests and diagnostics only.
    #[must_use]
    pub fn len(&self) -> usize {
        let mut entries = self.lock();
        Self::prune(&mut entries, self.ttl);
        entries.len()
    }

    /// Whether any token is outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Age every entry by `by`, so the expiry rule can be tested without the
    /// suite sleeping for ten seconds.
    #[cfg(test)]
    fn advance(&self, by: Duration) {
        for e in self.lock().iter_mut() {
            e.minted = e
                .minted
                .checked_sub(by)
                .expect("monotonic clock underflow — did this machine just boot?");
        }
    }

    fn prune(entries: &mut Vec<Entry>, ttl: Duration) {
        let now = Instant::now();
        entries.retain(|e| now.duration_since(e.minted) < ttl);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        // A poisoned mutex here means a previous holder panicked mid-mint. The
        // table is a plain `Vec` with no invariant that a panic could break,
        // and refusing every future connection would be a worse outcome than
        // carrying on.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_works_exactly_once() {
        let t = TokenTable::new(WS_TICKET_TTL);
        let ticket = t.mint();
        assert!(t.redeem(&ticket), "the first redemption must succeed");
        assert!(
            !t.redeem(&ticket),
            "a replayed ticket must be refused — this is the whole of ING-07"
        );
        assert!(t.is_empty());
    }

    #[test]
    fn a_ticket_expires() {
        let t = TokenTable::new(WS_TICKET_TTL);
        let ticket = t.mint();
        t.advance(WS_TICKET_TTL + Duration::from_millis(1));
        assert!(!t.redeem(&ticket));
    }

    #[test]
    fn a_ticket_that_has_not_expired_still_works() {
        let t = TokenTable::new(WS_TICKET_TTL);
        let ticket = t.mint();
        t.advance(WS_TICKET_TTL - Duration::from_millis(50));
        assert!(t.redeem(&ticket));
    }

    #[test]
    fn an_unminted_token_is_refused() {
        let t = TokenTable::new(WS_TICKET_TTL);
        let _live = t.mint();
        assert!(!t.redeem(""));
        assert!(!t.redeem(&random_token()));
        assert!(!t.redeem("../../etc/passwd"));
    }

    /// A guess that shares a prefix with a live ticket must be no closer to
    /// working than one that shares nothing.
    #[test]
    fn a_prefix_of_a_live_ticket_is_not_a_ticket() {
        let t = TokenTable::new(WS_TICKET_TTL);
        let ticket = t.mint();
        assert!(!t.redeem(&ticket[..ticket.len() - 1]));
        assert!(!t.redeem(&format!("{ticket}0")));
        assert!(t.redeem(&ticket), "the real ticket must be untouched");
    }

    #[test]
    fn redeeming_one_ticket_leaves_the_others_alone() {
        let t = TokenTable::new(WS_TICKET_TTL);
        let a = t.mint();
        let b = t.mint();
        assert!(t.redeem(&a));
        assert!(t.redeem(&b));
    }

    #[test]
    fn two_tickets_are_never_the_same() {
        let t = TokenTable::new(WS_TICKET_TTL);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            assert!(seen.insert(t.mint()), "tickets must not repeat");
        }
    }

    #[test]
    fn the_table_is_bounded() {
        let t = TokenTable::new(WS_TICKET_TTL);
        for _ in 0..(CAPACITY * 4) {
            let _ = t.mint();
        }
        assert!(t.len() <= CAPACITY);
    }

    #[test]
    fn the_handoff_token_has_its_own_clock() {
        let t = TokenTable::new(HANDOFF_TTL);
        let token = t.mint();
        // Past a WS ticket's life, still inside the handoff window.
        t.advance(WS_TICKET_TTL + Duration::from_secs(1));
        assert!(t.redeem(&token));

        let token = t.mint();
        t.advance(HANDOFF_TTL + Duration::from_millis(1));
        assert!(!t.redeem(&token), "ING-10: thirty seconds, then never");
    }
}
