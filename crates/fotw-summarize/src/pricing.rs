//! The dated price table (spec 8.2, SUM-11).
//!
//! **Never hardcode a price without an effective-from date.** Spec 7.1 says it
//! about STT and spec 8.2 proves why for LLMs in the same breath: Sonnet 5's
//! introductory $2/$10 **expires 2026-08-31**, three weeks after the spec was
//! written, reverting to $3/$15. A single `const SONNET_INPUT` somewhere is a
//! cost estimator that silently understates by a third on a date nobody
//! diaried.
//!
//! So the table is a list of `(model, effective_from)` rows and
//! [`price_for`] picks the row in force on a given date. Adding the next
//! repricing is appending a row, not editing one — which keeps a meeting
//! summarized last month costed at what it actually cost.
//!
//! Dates are ISO-8601 strings compared lexicographically, which is correct for
//! that format and keeps a date library out of the dependency graph for what
//! is fundamentally a table lookup.

use crate::adapter::Usage;
use crate::capabilities::CacheTtl;

/// One model's price from one date onwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    /// Model id.
    pub model: &'static str,
    /// ISO-8601 date this price took effect.
    pub effective_from: &'static str,
    /// USD per million input tokens.
    pub input_per_mtok_usd: f64,
    /// USD per million output tokens.
    pub output_per_mtok_usd: f64,
}

/// Published prices, oldest first per model (spec 8.2).
///
/// The Sonnet 5 row for 2026-08-31 is the repricing spec 8.2 asks to ship
/// *now* rather than on the day. It is not a guess: the spec states the
/// introductory rate expires on that date and reverts to $3/$15.
pub const PRICES: &[Price] = &[
    Price {
        model: crate::capabilities::MODEL_OPUS_5,
        effective_from: "2026-01-01",
        input_per_mtok_usd: 5.0,
        output_per_mtok_usd: 25.0,
    },
    Price {
        model: crate::capabilities::MODEL_SONNET_5,
        effective_from: "2026-01-01",
        input_per_mtok_usd: 2.0,
        output_per_mtok_usd: 10.0,
    },
    Price {
        model: crate::capabilities::MODEL_SONNET_5,
        effective_from: "2026-08-31",
        input_per_mtok_usd: 3.0,
        output_per_mtok_usd: 15.0,
    },
    Price {
        model: crate::capabilities::MODEL_HAIKU_4_5,
        effective_from: "2026-01-01",
        input_per_mtok_usd: 1.0,
        output_per_mtok_usd: 5.0,
    },
];

/// Multiplier on a 5-minute cache write.
pub const CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;
/// Multiplier on a 1-hour cache write.
pub const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;
/// Multiplier on a cache read, either TTL.
pub const CACHE_READ_MULTIPLIER: f64 = 0.1;

/// The price in force for `model` on `date`, or `None` if we do not know one.
///
/// `None` rather than a fallback: showing a user a confidently wrong cost is
/// worse than showing them "unknown", because they act on it (SUM-11 exists
/// because users spend their own money).
#[must_use]
pub fn price_for(model: &str, date: &str) -> Option<&'static Price> {
    PRICES
        .iter()
        .rfind(|price| price.model == model && price.effective_from <= date)
}

/// Write multiplier for a TTL.
#[must_use]
pub fn cache_write_multiplier(ttl: CacheTtl) -> f64 {
    match ttl {
        CacheTtl::None => 1.0,
        CacheTtl::FiveMinutes => CACHE_WRITE_5M_MULTIPLIER,
        CacheTtl::OneHour => CACHE_WRITE_1H_MULTIPLIER,
    }
}

/// USD for one call's usage.
#[must_use]
pub fn cost_usd(price: &Price, usage: &Usage, write_ttl: CacheTtl) -> f64 {
    let per_input = price.input_per_mtok_usd / 1_000_000.0;
    let per_output = price.output_per_mtok_usd / 1_000_000.0;

    usage.input_tokens as f64 * per_input
        + usage.cache_creation_input_tokens as f64 * per_input * cache_write_multiplier(write_ttl)
        + usage.cache_read_input_tokens as f64 * per_input * CACHE_READ_MULTIPLIER
        + usage.output_tokens as f64 * per_output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{MODEL_HAIKU_4_5, MODEL_OPUS_5, MODEL_SONNET_5};

    #[test]
    fn sonnet_5_reprices_on_the_last_day_of_august_2026() {
        // Spec 8.2's warning, encoded rather than diarized. If this ever fails
        // it is because somebody edited a row instead of appending one, and a
        // meeting summarized in July is now costed at August's prices.
        let before = price_for(MODEL_SONNET_5, "2026-08-30").expect("priced");
        assert!((before.input_per_mtok_usd - 2.0).abs() < f64::EPSILON);
        assert!((before.output_per_mtok_usd - 10.0).abs() < f64::EPSILON);

        let after = price_for(MODEL_SONNET_5, "2026-08-31").expect("priced");
        assert!((after.input_per_mtok_usd - 3.0).abs() < f64::EPSILON);
        assert!((after.output_per_mtok_usd - 15.0).abs() < f64::EPSILON);

        let later = price_for(MODEL_SONNET_5, "2027-01-15").expect("priced");
        assert_eq!(later, after, "the newest row in force must win");
    }

    #[test]
    fn an_unknown_model_or_a_date_before_any_row_has_no_price() {
        assert!(price_for("gpt-9", "2026-08-11").is_none());
        assert!(price_for(MODEL_OPUS_5, "2025-12-31").is_none());
    }

    #[test]
    fn the_frontier_and_extraction_models_are_priced_as_spec_8_2_states() {
        let opus = price_for(MODEL_OPUS_5, "2026-08-11").expect("priced");
        assert!((opus.input_per_mtok_usd - 5.0).abs() < f64::EPSILON);
        assert!((opus.output_per_mtok_usd - 25.0).abs() < f64::EPSILON);

        let haiku = price_for(MODEL_HAIKU_4_5, "2026-08-11").expect("priced");
        assert!((haiku.input_per_mtok_usd - 1.0).abs() < f64::EPSILON);
        assert!((haiku.output_per_mtok_usd - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_cache_ttl_arithmetic_from_spec_8_4_comes_out_as_the_spec_says() {
        // The counterintuitive result the pipeline's default rests on, checked
        // numerically instead of taken on faith: on a 20k-token transcript at
        // Opus 5 rates, a 1-hour TTL costs MORE than not caching at all.
        let opus = price_for(MODEL_OPUS_5, "2026-08-11").expect("priced");
        let transcript = 20_000;

        // No cache: both calls pay full input price.
        let uncached = 2.0
            * cost_usd(
                opus,
                &Usage {
                    input_tokens: transcript,
                    ..Usage::default()
                },
                CacheTtl::None,
            );

        // Cached: Call A writes the prefix, Call B reads it.
        let cached = |ttl| {
            let write = cost_usd(
                opus,
                &Usage {
                    cache_creation_input_tokens: transcript,
                    ..Usage::default()
                },
                ttl,
            );
            let read = cost_usd(
                opus,
                &Usage {
                    cache_read_input_tokens: transcript,
                    ..Usage::default()
                },
                ttl,
            );
            write + read
        };

        let five_minutes = cached(CacheTtl::FiveMinutes);
        let one_hour = cached(CacheTtl::OneHour);

        assert!((uncached - 0.20).abs() < 1e-9, "uncached = {uncached}");
        assert!((five_minutes - 0.135).abs() < 1e-9, "5m = {five_minutes}");
        assert!((one_hour - 0.21).abs() < 1e-9, "1h = {one_hour}");

        assert!(five_minutes < uncached, "5m should beat no cache");
        assert!(
            one_hour > uncached,
            "spec 8.4's headline: 1h is worse than not caching for two calls"
        );
    }

    #[test]
    fn a_second_read_is_what_makes_the_one_hour_ttl_pay() {
        // Which is exactly the condition `pipeline::cache_ttl_for` upgrades on.
        let opus = price_for(MODEL_OPUS_5, "2026-08-11").expect("priced");
        let transcript = 20_000;
        let read = cost_usd(
            opus,
            &Usage {
                cache_read_input_tokens: transcript,
                ..Usage::default()
            },
            CacheTtl::OneHour,
        );
        let write = cost_usd(
            opus,
            &Usage {
                cache_creation_input_tokens: transcript,
                ..Usage::default()
            },
            CacheTtl::OneHour,
        );
        let uncached_per_call = cost_usd(
            opus,
            &Usage {
                input_tokens: transcript,
                ..Usage::default()
            },
            CacheTtl::None,
        );

        // Three calls: one write, two reads, against three full-price calls.
        assert!(write + 2.0 * read < 3.0 * uncached_per_call);
    }

    #[test]
    fn output_tokens_are_priced_at_the_output_rate() {
        let opus = price_for(MODEL_OPUS_5, "2026-08-11").expect("priced");
        let cost = cost_usd(
            opus,
            &Usage {
                output_tokens: 1_000_000,
                ..Usage::default()
            },
            CacheTtl::None,
        );
        assert!((cost - 25.0).abs() < 1e-9);
    }
}
