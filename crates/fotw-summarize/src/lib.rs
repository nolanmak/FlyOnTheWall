//! `fotw-summarize` — transcript to grounded notes (spec 8).
//!
//! See `docs/REQUIREMENTS.md` §8.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capabilities;
pub mod document;
pub mod error;
pub mod hash;
pub mod prompt;
pub mod schema;
pub mod testing;
pub mod tokens;
pub mod transport;
pub mod validate;
