//! The 32-byte database master key, and the only place it is ever formatted.

use std::fmt;

use crate::error::{Result, StoreError};

/// A raw 32-byte SQLCipher key.
///
/// # Why raw, and not a passphrase
///
/// `PRAGMA key = 'some passphrase'` makes SQLCipher run 256,000 rounds of
/// PBKDF2-HMAC-SHA512 **on every connection open**. The daemon opens a writer
/// and N readers, and reopens after every crash-recovery pass, so that cost is
/// paid over and over for no security benefit: our key already comes from the
/// OS CSPRNG and lives in the keychain (docs/REQUIREMENTS.md 10). The raw-key
/// form `x'<64 hex>'` tells SQLCipher to use the bytes directly and skip key
/// derivation entirely.
///
/// # Why it is its own type
///
/// So the never-log rules in §10 can be enforced by the compiler rather than
/// by reviewer diligence: [`fmt::Debug`] and [`fmt::Display`] both redact, and
/// the only way to get at the material is [`DbKey::pragma_literal`], which has
/// exactly one call site.
#[derive(Clone, PartialEq, Eq)]
pub struct DbKey([u8; 32]);

impl DbKey {
    /// Wrap 32 bytes of key material.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Wrap key material of unknown length, rejecting anything but 32 bytes.
    ///
    /// SQLCipher silently zero-pads a short raw key rather than failing, which
    /// would turn a truncated keychain read into a weaker database that still
    /// opens. This is the guard against that.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            StoreError::InvalidKey(format!("expected 32 bytes, got {}", bytes.len()))
        })?;
        Ok(Self(arr))
    }

    /// The `x'<64 hex>'` literal for `PRAGMA key`.
    ///
    /// Returned as an owned `String` that the caller is expected to drop
    /// promptly. It is not `Display` on purpose — an accidental `{}` in a log
    /// format string must not be able to leak the key.
    #[must_use]
    pub fn pragma_literal(&self) -> String {
        let mut out = String::with_capacity(2 + 64 + 1);
        out.push_str("x'");
        for b in self.0 {
            use fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out.push('\'');
        out
    }
}

impl Drop for DbKey {
    /// Best-effort zeroize. `write_volatile` keeps the compiler from eliding
    /// a write to memory it can prove is never read again.
    fn drop(&mut self) {
        for b in &mut self.0 {
            // SAFETY: `b` is a live, aligned, writable `u8`.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl fmt::Debug for DbKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DbKey(<redacted>)")
    }
}

impl fmt::Display for DbKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}
