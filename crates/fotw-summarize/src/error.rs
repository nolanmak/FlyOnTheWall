//! The summarization error taxonomy.
//!
//! Mirrors the shape of `fotw_stt::SttError`: the layer above reacts to the
//! class, never to a provider's status code. The one variant worth reading
//! twice is [`SummarizeError::CitationsWithStructuredOutput`] — that
//! combination is a **400 from the Anthropic API** (spec 8.4), so we refuse it
//! locally rather than spend a round trip discovering it.

use thiserror::Error;

/// Everything that can go wrong between a transcript and a summary.
#[derive(Debug, Error)]
pub enum SummarizeError {
    /// Citations and `output_config.format` were requested on the same call.
    ///
    /// Server-side this is an HTTP 400. It is a *programming* error, not a
    /// runtime condition: the two-call pipeline (spec 8.4) exists precisely
    /// because the two features are mutually exclusive, so reaching this means
    /// a caller collapsed Call A and Call B into one.
    #[error(
        "citations and structured output are mutually exclusive on this provider (HTTP 400); \
         use the two-call pipeline (spec 8.4)"
    )]
    CitationsWithStructuredOutput,

    /// A capability the request needs is not offered by the adapter.
    ///
    /// Carries the flag name so the message names the *capability*, never the
    /// provider — the spec 8.2 rule that downstream code branches on
    /// capabilities applies to error messages too.
    #[error("adapter does not support the required capability `{capability}`")]
    UnsupportedCapability {
        /// The capability flag that was required but absent.
        capability: &'static str,
    },

    /// The transcript does not fit even the chunked path.
    #[error("transcript needs {needed} tokens but the adapter's usable context is {usable}")]
    ContextOverflow {
        /// Tokens the transcript is estimated to need.
        needed: usize,
        /// Tokens the adapter can actually use.
        usable: usize,
    },

    /// The transport failed before an HTTP status existed: DNS, TLS, reset.
    #[error("transport failure: {0}")]
    Transport(String),

    /// The provider answered with a non-2xx status.
    #[error("provider returned HTTP {status}: {body}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// The response body, truncated by the transport if it was large.
        body: String,
    },

    /// The provider's response did not have the shape the adapter expects.
    #[error("could not decode provider response: {0}")]
    Decode(String),

    /// The model emitted something that is not valid against the schema we
    /// sent. Distinct from [`SummarizeError::Decode`] because it is the
    /// *model's* failure rather than the adapter's, and the pipeline may retry
    /// it where a decode failure is a bug.
    #[error("model output did not match the extraction schema: {0}")]
    SchemaViolation(String),

    /// The response stopped for a reason that means the content is incomplete.
    ///
    /// SUM-10: check `stop_reason` before reading `content`. A `max_tokens`
    /// stop leaves a truncated markdown document that would otherwise be
    /// rendered as if it were finished.
    #[error("generation stopped early: {0}")]
    Truncated(String),
}

impl SummarizeError {
    /// Whether retrying the identical request could plausibly succeed.
    ///
    /// Used by the daemon's retry wrapper. Deliberately conservative: a
    /// mutually-exclusive-features error is never retryable, because the
    /// request is wrong and will be wrong again.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::SchemaViolation(_) => true,
            Self::Http { status, .. } => *status == 429 || *status >= 500,
            Self::CitationsWithStructuredOutput
            | Self::UnsupportedCapability { .. }
            | Self::ContextOverflow { .. }
            | Self::Decode(_)
            | Self::Truncated(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutually_exclusive_features_are_never_retryable() {
        assert!(!SummarizeError::CitationsWithStructuredOutput.is_retryable());
    }

    #[test]
    fn rate_limit_and_server_errors_are_retryable_but_bad_request_is_not() {
        assert!(
            SummarizeError::Http {
                status: 429,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            SummarizeError::Http {
                status: 503,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            !SummarizeError::Http {
                status: 400,
                body: String::new()
            }
            .is_retryable()
        );
    }
}
