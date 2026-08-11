//! The provider capability descriptor (spec 8.2).
//!
//! **Every branch in this crate is on a flag in [`Capabilities`], never on a
//! provider or model name.** That is the whole reason the type exists. A
//! `if model.starts_with("claude")` scattered through the pipeline is how a
//! codebase ends up unable to add a provider without touching ten call sites,
//! and how a local Ollama model silently gets sent a `citations` block it
//! cannot honour.
//!
//! The field names and the wire shape match the TypeScript block in spec 8.2
//! byte for byte, so the settings UI can round-trip them.

use serde::{Deserialize, Serialize};

/// The longest prompt-cache TTL a provider supports.
///
/// A *capability*, not a request parameter — see [`CacheTtl`] for the per-call
/// choice. Ordered worst-to-best so `>=` comparisons read naturally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCache {
    /// No prompt caching at all. Every call pays full input price.
    None,
    /// 5-minute ephemeral cache: 1.25× write, 0.1× read.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// 1-hour extended cache: 2× write, 0.1× read.
    #[serde(rename = "1h")]
    OneHour,
}

/// The cache TTL requested for one call's prefix.
///
/// See [`Capabilities::clamp_ttl`] for why this is separate from
/// [`PromptCache`], and `pipeline::cache_ttl_for` for why the default is
/// **5 minutes and not 1 hour** — the arithmetic is counterintuitive enough
/// that it is written out at that call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheTtl {
    /// Do not cache this prefix.
    None,
    /// `ttl: "5m"`.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// `ttl: "1h"`.
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    /// The literal the provider expects, or `None` when caching is off.
    #[must_use]
    pub fn wire_value(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::FiveMinutes => Some("5m"),
            Self::OneHour => Some("1h"),
        }
    }
}

/// How hard the model should think, when the provider exposes the control.
///
/// Spec 8.2: on Opus 5 this rides in `output_config.effort`. `temperature`,
/// `top_p`, `top_k` and `budget_tokens` all return 400 and are deliberately
/// absent from this crate's vocabulary — there is no type that can express
/// them, which is the cheapest possible enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Cheapest, for extraction-shaped work.
    Low,
    /// The default.
    Medium,
    /// The `quality` preset.
    High,
}

impl Effort {
    /// The literal the provider expects.
    #[must_use]
    pub fn wire_value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// What a summarization provider can and cannot do (spec 8.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// The provider extracts `cited_text` server-side and returns block
    /// locations. When false the local path is prompt-based `[[seg:N]]`
    /// markers with all the weaker guarantees that implies (SUM-13).
    pub native_citations: bool,
    /// The provider enforces a JSON schema server-side. When false, Call B's
    /// output must be parsed defensively and validated locally.
    pub strict_json_schema: bool,
    /// The longest cache TTL on offer.
    pub prompt_cache: PromptCache,
    /// Context window we are willing to fill, in tokens. Not the advertised
    /// maximum — the number the chunking policy is allowed to plan against.
    pub usable_context_tokens: usize,
    /// Hard cap on a single response.
    pub max_output_tokens: usize,
    /// `output_config.effort` is accepted.
    pub supports_effort: bool,
    /// Extended thinking is available. We never *disable* it (spec 8.2), so
    /// this only gates whether the field may be mentioned at all.
    pub supports_thinking: bool,
}

/// The fraction of usable context above which map-reduce engages (spec 8.1).
pub const SINGLE_SHOT_FRACTION: f64 = 0.6;

/// The fraction of usable context each map-reduce chunk is packed to (spec 8.1).
pub const CHUNK_FRACTION: f64 = 0.35;

impl Capabilities {
    /// Tokens that may go into a single-shot call.
    ///
    /// Spec 8.1: map-reduce engages only above `usable_context * 0.6`. The
    /// headroom is not slack — it is the system prompt, the user's notes, the
    /// instruction block and the response itself, none of which are in the
    /// transcript's token count.
    #[must_use]
    pub fn single_shot_budget_tokens(&self) -> usize {
        fraction_of(self.usable_context_tokens, SINGLE_SHOT_FRACTION)
    }

    /// Tokens per map-reduce chunk (spec 8.1: `usable_context * 0.35`).
    #[must_use]
    pub fn chunk_budget_tokens(&self) -> usize {
        fraction_of(self.usable_context_tokens, CHUNK_FRACTION)
    }

    /// Whether a transcript of `tokens` can be summarized in one call.
    #[must_use]
    pub fn fits_single_shot(&self, tokens: usize) -> bool {
        tokens < self.single_shot_budget_tokens()
    }

    /// Reduce a requested TTL to what the provider actually offers.
    ///
    /// Asking a provider for `1h` when it only does `5m` is a 400 on some
    /// backends and a silently-ignored field on others; both are worse than
    /// asking for what exists.
    #[must_use]
    pub fn clamp_ttl(&self, requested: CacheTtl) -> CacheTtl {
        match (self.prompt_cache, requested) {
            (PromptCache::None, _) | (_, CacheTtl::None) => CacheTtl::None,
            (PromptCache::FiveMinutes, _) => CacheTtl::FiveMinutes,
            (PromptCache::OneHour, ttl) => ttl,
        }
    }

    /// Capabilities of the Anthropic frontier models (spec 8.2).
    ///
    /// 1M context, 128K max output. `usable_context_tokens` is set to the full
    /// window: spec 8.1's 0.6 factor already reserves the headroom, and
    /// double-discounting it here would engage map-reduce on meetings that fit
    /// comfortably, which spec 8.1 explicitly calls a local-model-only path.
    #[must_use]
    pub fn anthropic_frontier() -> Self {
        Self {
            native_citations: true,
            strict_json_schema: true,
            prompt_cache: PromptCache::OneHour,
            usable_context_tokens: 1_000_000,
            max_output_tokens: 128_000,
            supports_effort: true,
            supports_thinking: true,
        }
    }

    /// Capabilities of a self-hosted model behind Ollama / LM Studio.
    ///
    /// No server-side citations, no strict schema, no prompt cache, and a
    /// context small enough that map-reduce is the normal path rather than the
    /// exception — which is exactly what spec 8.1 means by calling map-reduce
    /// a local-model code path. 32K is the honest floor across the tested
    /// allowlist (SUM-13); a model with more should report more.
    #[must_use]
    pub fn local_default() -> Self {
        Self {
            native_citations: false,
            strict_json_schema: false,
            prompt_cache: PromptCache::None,
            usable_context_tokens: 32_768,
            max_output_tokens: 4_096,
            supports_effort: false,
            supports_thinking: false,
        }
    }
}

/// `value * fraction`, never panicking on a huge context.
///
/// Token budgets are far below f64's exact-integer range and are advisory
/// thresholds, so a one-token rounding difference is immaterial.
fn fraction_of(value: usize, fraction: f64) -> usize {
    (value as f64 * fraction) as usize
}

/// The user-facing quality/cost presets (spec 8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    /// `claude-opus-5`, effort high.
    Quality,
    /// `claude-opus-5`, effort medium. The default.
    Balanced,
    /// `claude-sonnet-5`, effort low.
    Cheap,
    /// A user-selected Ollama / LM Studio model, no effort control.
    Local,
}

impl Default for Preset {
    fn default() -> Self {
        Self::Balanced
    }
}

/// Model id for the augment/summary call (Call A).
pub const MODEL_OPUS_5: &str = "claude-opus-5";
/// Model id for the `cheap` preset's prose call.
pub const MODEL_SONNET_5: &str = "claude-sonnet-5";
/// Model id for structured extraction (Call B) under the `cheap` preset.
///
/// Spec 8.4: extraction is a low-difficulty task at $1/$5 against $5/$25, so
/// running it on the frontier model is money spent for no measurable gain.
pub const MODEL_HAIKU_4_5: &str = "claude-haiku-4-5";

impl Preset {
    /// The model that runs Call A (cited prose) under this preset.
    #[must_use]
    pub fn prose_model(self) -> Option<&'static str> {
        match self {
            Self::Quality | Self::Balanced => Some(MODEL_OPUS_5),
            Self::Cheap => Some(MODEL_SONNET_5),
            // Deliberately not a default string: the local model is whatever
            // the user picked from the tested allowlist (SUM-13), and inventing
            // an id here would produce a confusing 404 from their own server.
            Self::Local => None,
        }
    }

    /// The model that runs Call B (structured extraction) under this preset.
    #[must_use]
    pub fn extraction_model(self) -> Option<&'static str> {
        match self {
            Self::Quality | Self::Balanced => Some(MODEL_OPUS_5),
            Self::Cheap => Some(MODEL_HAIKU_4_5),
            Self::Local => None,
        }
    }

    /// The effort level, where the provider supports one.
    #[must_use]
    pub fn effort(self) -> Option<Effort> {
        match self {
            Self::Quality => Some(Effort::High),
            Self::Balanced => Some(Effort::Medium),
            Self::Cheap => Some(Effort::Low),
            Self::Local => None,
        }
    }

    /// Capabilities implied by the preset.
    #[must_use]
    pub fn capabilities(self) -> Capabilities {
        match self {
            Self::Quality | Self::Balanced | Self::Cheap => Capabilities::anthropic_frontier(),
            Self::Local => Capabilities::local_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_table_matches_spec_8_2() {
        assert_eq!(Preset::Quality.prose_model(), Some(MODEL_OPUS_5));
        assert_eq!(Preset::Quality.effort(), Some(Effort::High));
        assert_eq!(Preset::Balanced.prose_model(), Some(MODEL_OPUS_5));
        assert_eq!(Preset::Balanced.effort(), Some(Effort::Medium));
        assert_eq!(Preset::Cheap.prose_model(), Some(MODEL_SONNET_5));
        assert_eq!(Preset::Cheap.effort(), Some(Effort::Low));
        assert_eq!(Preset::Local.prose_model(), None);
        assert_eq!(Preset::Local.effort(), None);
    }

    #[test]
    fn default_preset_is_balanced() {
        assert_eq!(Preset::default(), Preset::Balanced);
    }

    #[test]
    fn cheap_preset_runs_extraction_on_haiku() {
        // Spec 8.4: Call B is a low-difficulty task and does not need the
        // frontier model's price.
        assert_eq!(Preset::Cheap.extraction_model(), Some(MODEL_HAIKU_4_5));
    }

    #[test]
    fn map_reduce_threshold_is_sixty_percent_of_usable_context() {
        let caps = Capabilities {
            usable_context_tokens: 100_000,
            ..Capabilities::anthropic_frontier()
        };
        assert_eq!(caps.single_shot_budget_tokens(), 60_000);
        assert!(caps.fits_single_shot(59_999));
        assert!(!caps.fits_single_shot(60_000));
        assert!(!caps.fits_single_shot(60_001));
    }

    #[test]
    fn chunk_budget_is_thirty_five_percent_of_usable_context() {
        let caps = Capabilities {
            usable_context_tokens: 100_000,
            ..Capabilities::anthropic_frontier()
        };
        assert_eq!(caps.chunk_budget_tokens(), 35_000);
    }

    #[test]
    fn a_one_hour_transcript_fits_single_shot_on_the_frontier_models() {
        // Spec 8.1: 18k-25k tokens for an hour, 55k-75k for three hours. Both
        // are far under 600k, which is why map-reduce is not the default path.
        let caps = Capabilities::anthropic_frontier();
        assert!(caps.fits_single_shot(25_000));
        assert!(caps.fits_single_shot(75_000));
    }

    #[test]
    fn a_local_model_engages_map_reduce_on_an_ordinary_meeting() {
        let caps = Capabilities::local_default();
        assert!(!caps.fits_single_shot(25_000));
    }

    #[test]
    fn ttl_is_clamped_to_what_the_provider_offers() {
        let none = Capabilities {
            prompt_cache: PromptCache::None,
            ..Capabilities::local_default()
        };
        assert_eq!(none.clamp_ttl(CacheTtl::OneHour), CacheTtl::None);

        let five = Capabilities {
            prompt_cache: PromptCache::FiveMinutes,
            ..Capabilities::anthropic_frontier()
        };
        assert_eq!(five.clamp_ttl(CacheTtl::OneHour), CacheTtl::FiveMinutes);

        let hour = Capabilities::anthropic_frontier();
        assert_eq!(hour.clamp_ttl(CacheTtl::OneHour), CacheTtl::OneHour);
        assert_eq!(hour.clamp_ttl(CacheTtl::None), CacheTtl::None);
    }

    #[test]
    fn capabilities_round_trip_through_the_settings_wire_shape() {
        let caps = Capabilities::anthropic_frontier();
        let json = serde_json::to_value(&caps).expect("serialize");
        assert_eq!(json["nativeCitations"], serde_json::json!(true));
        assert_eq!(json["promptCache"], serde_json::json!("1h"));
        assert_eq!(json["usableContextTokens"], serde_json::json!(1_000_000));
        let back: Capabilities = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, caps);
    }

    #[test]
    fn local_capabilities_deny_the_two_server_side_guardrails() {
        let caps = Capabilities::local_default();
        assert!(!caps.native_citations);
        assert!(!caps.strict_json_schema);
        assert_eq!(caps.prompt_cache, PromptCache::None);
    }
}
