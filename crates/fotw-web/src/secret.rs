//! The per-start secret, and the tokens derived from the same CSPRNG.
//!
//! docs/REQUIREMENTS.md 10.1 **ING-05**: a 256-bit CSPRNG secret minted once
//! per daemon start and compared with `subtle::ConstantTimeEq`. It is the
//! control that catches everything ING-02 misses, so the two ways it usually
//! dies are both closed here:
//!
//! * **A predictable source.** Nothing in this module reaches for a userspace
//!   PRNG. [`getrandom::fill`] is the OS CSPRNG (`getentropy` on macOS,
//!   `getrandom(2)` on Linux) and a failure is a panic, not a fallback — a
//!   daemon that cannot get 32 random bytes must not come up serving
//!   transcripts behind a guessable token.
//! * **A byte-at-a-time comparison.** `a == b` on `&str` returns at the first
//!   differing byte. Against a loopback server a page can time that, and 64
//!   hex characters recovered one at a time is 64 × 16 requests, not 2^256.
//!   [`Secret::matches`] is [`subtle::ConstantTimeEq`] over the whole buffer.

use subtle::ConstantTimeEq;

/// Bytes of entropy behind a secret or token. 256 bits, per ING-05.
pub const TOKEN_BYTES: usize = 32;
/// Length of the hex rendering that actually travels in headers and URLs.
pub const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// A 256-bit secret, held only in its hex form because that is the only form
/// it is ever compared against.
///
/// `Debug` is redacted: §10's never-log rules mean a stray `{:?}` on the app
/// state must not put the bearer token in a log line, and the cheapest way to
/// guarantee that is to make the type incapable of printing itself.
pub struct Secret {
    hex: [u8; TOKEN_HEX_LEN],
}

impl Secret {
    /// Mint a fresh secret from the OS CSPRNG.
    ///
    /// # Panics
    ///
    /// If the OS cannot supply entropy. See the module docs: degrading to a
    /// weaker source here would be worse than not starting.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            hex: random_hex_array(),
        }
    }

    /// Build from a known hex string. Tests only — real secrets come from
    /// [`Secret::generate`].
    #[must_use]
    pub fn from_hex(hex: &str) -> Option<Self> {
        let bytes: [u8; TOKEN_HEX_LEN] = hex.as_bytes().try_into().ok()?;
        if bytes.iter().any(|b| !b.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self { hex: bytes })
    }

    /// The value the client puts in `Authorization: Bearer`.
    ///
    /// Named `expose_` so that every call site reads as a deliberate
    /// disclosure — there are exactly two, the state file and the handoff
    /// exchange.
    #[must_use]
    pub fn expose_hex(&self) -> String {
        // Infallible: the buffer is ASCII hex by construction.
        String::from_utf8_lossy(&self.hex).into_owned()
    }

    /// Whether `presented` is this secret, in time independent of how much of
    /// it is correct.
    ///
    /// A length mismatch short-circuits, which leaks only the length — a
    /// public constant ([`TOKEN_HEX_LEN`]) — and never a prefix.
    #[must_use]
    pub fn matches(&self, presented: &[u8]) -> bool {
        self.hex.ct_eq(presented).into()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Not a substitute for the `zeroize` crate — an optimiser is allowed
        // to elide a dead store — but `black_box` makes the buffer observably
        // used afterwards, which is what keeps the write. Costs nothing and
        // shortens the window in which a core dump contains the token.
        self.hex.fill(0);
        std::hint::black_box(&self.hex);
    }
}

/// A fresh 256-bit token, hex-encoded: WS tickets (ING-07) and the launch
/// handoff (ING-10).
///
/// # Panics
///
/// If the OS CSPRNG fails. See [`Secret::generate`].
#[must_use]
pub fn random_token() -> String {
    String::from_utf8_lossy(&random_hex_array()).into_owned()
}

/// Constant-time equality for two tokens of the same length.
///
/// Used by the ticket table, where an early-return compare would leak how many
/// leading characters of a live ticket a guess got right.
#[must_use]
pub fn tokens_match(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn random_hex_array() -> [u8; TOKEN_HEX_LEN] {
    let mut raw = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut raw).expect("the OS CSPRNG must be available");
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = [0u8; TOKEN_HEX_LEN];
    for (i, byte) in raw.iter().enumerate() {
        hex[i * 2] = DIGITS[usize::from(byte >> 4)];
        hex[i * 2 + 1] = DIGITS[usize::from(byte & 0x0f)];
    }
    raw.fill(0);
    std::hint::black_box(&raw);
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_64_hex_characters() {
        let s = Secret::generate();
        let hex = s.expose_hex();
        assert_eq!(hex.len(), TOKEN_HEX_LEN);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Not a randomness test — it cannot be — but it does catch the failure
    /// that actually happens: a constant, a counter, or a PRNG seeded once.
    #[test]
    fn two_secrets_differ() {
        let a = Secret::generate().expose_hex();
        let b = Secret::generate().expose_hex();
        assert_ne!(a, b);
        assert_ne!(random_token(), random_token());
    }

    #[test]
    fn a_secret_matches_only_itself() {
        let s = Secret::generate();
        let hex = s.expose_hex();
        assert!(s.matches(hex.as_bytes()));
        assert!(!s.matches(b""));
        assert!(!s.matches(random_token().as_bytes()));
        // A correct prefix is not a match: the whole point of ING-05.
        assert!(!s.matches(&hex.as_bytes()[..TOKEN_HEX_LEN - 1]));
        let mut nearly = hex.into_bytes();
        nearly[TOKEN_HEX_LEN - 1] = if nearly[TOKEN_HEX_LEN - 1] == b'a' {
            b'b'
        } else {
            b'a'
        };
        assert!(!s.matches(&nearly));
    }

    #[test]
    fn debug_does_not_print_the_secret() {
        let s = Secret::generate();
        let printed = format!("{s:?}");
        assert!(!printed.contains(&s.expose_hex()));
        assert!(printed.contains("redacted"));
    }

    #[test]
    fn from_hex_rejects_anything_that_is_not_a_token() {
        assert!(Secret::from_hex("").is_none());
        assert!(Secret::from_hex("zz").is_none());
        assert!(Secret::from_hex(&"g".repeat(TOKEN_HEX_LEN)).is_none());
        assert!(Secret::from_hex(&"a".repeat(TOKEN_HEX_LEN)).is_some());
    }
}
