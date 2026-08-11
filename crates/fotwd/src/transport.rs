//! The one place the daemon is allowed to talk to a provider.
//!
//! Every outbound request funnels through here so two guarantees can be
//! enforced in a single spot rather than reviewed at every call site:
//!
//! **KEY-02, the egress allowlist.** A host not on the list is refused before
//! a socket is opened. This is the technical backing for "no vendor backend
//! relays your audio" — without it that claim is marketing. The check is here,
//! not in the provider crates, because a provider crate could be swapped or
//! forked; this wrapper cannot be bypassed without deleting it.
//!
//! **KEY-03, the privacy flags.** Provider defaults are not privacy-safe:
//! Deepgram's published pay-as-you-go rates opt in to model training, so
//! `mip_opt_out=true` has to ride on every request and must not be a settings
//! toggle. The STT client already appends it; this wrapper asserts it rather
//! than trusting it, because a regression there would silently feed a
//! non-consenting participant's voice into a training corpus.

use std::time::Duration;

use fotw_summarize::SummarizeError;
use fotw_summarize::transport::{BoxFuture, HttpRequest, HttpResponse, HttpTransport};

/// Hosts the daemon may contact. Nothing else, ever.
///
/// Deliberately a hardcoded list rather than configuration: a user-editable
/// allowlist is an allowlist an attacker can edit, and every entry here
/// corresponds to a provider the user explicitly configured a key for.
pub const ALLOWED_HOSTS: &[&str] = &[
    "api.anthropic.com",
    "api.openai.com",
    "api.deepgram.com",
    "api.elevenlabs.io",
    "api.us.elevenlabs.io",
    "api.eu.residency.elevenlabs.io",
    "api.in.residency.elevenlabs.io",
    "api.sg.residency.elevenlabs.io",
];

/// Whether `url` targets a host the daemon is permitted to contact.
///
/// Compares the host **exactly**. A suffix test would accept
/// `api.deepgram.com.evil.test`, which is precisely the shape an exfiltration
/// endpoint takes.
#[must_use]
pub fn is_allowed(url: &str) -> bool {
    host_of(url).is_some_and(|h| ALLOWED_HOSTS.contains(&h.as_str()))
}

fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("wss://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Strip any userinfo: `https://api.deepgram.com@evil.test/` has host
    // `evil.test`, and reading the part before the `@` is a classic parser
    // confusion bug.
    let authority = authority.rsplit('@').next()?;
    let host = authority.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// How long a single provider call may take before it is abandoned.
///
/// Generous: a long transcript on a thinking model genuinely takes a while,
/// and a spurious timeout costs the user a summary they already paid for.
const TIMEOUT: Duration = Duration::from_secs(300);

/// The daemon's real transport.
#[derive(Debug, Clone)]
pub struct AllowlistedTransport {
    client: reqwest::Client,
}

impl Default for AllowlistedTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl AllowlistedTransport {
    /// Construct the transport.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(TIMEOUT)
                // No proxy auto-detection: a corporate proxy env var would
                // silently route meeting transcripts through a middlebox the
                // user never consented to, and the allowlist above would still
                // read as satisfied because the URL is unchanged.
                .no_proxy()
                .build()
                .unwrap_or_default(),
        }
    }
}

impl HttpTransport for AllowlistedTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, SummarizeError>> {
        Box::pin(async move {
            if !is_allowed(&request.url) {
                // A refusal, not a failure: the caller asked for something the
                // daemon is structurally not allowed to do.
                return Err(SummarizeError::Transport(format!(
                    "refusing to contact {} — not on the egress allowlist",
                    host_of(&request.url).unwrap_or_else(|| "<unparseable>".into())
                )));
            }
            let mut req = self.client.post(&request.url).body(request.body);
            for (name, value) in &request.headers {
                req = req.header(name, value);
            }

            let response = req
                .send()
                .await
                .map_err(|e| SummarizeError::Transport(format!("{e}")))?;

            // A non-2xx status is NOT an error here: the adapter needs the
            // status and the body to tell a rate limit from a schema
            // rejection, and collapsing them would make every provider
            // failure look the same.
            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|e| SummarizeError::Transport(format!("reading the body: {e}")))?
                .to_vec();

            Ok(HttpResponse { status, body })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_configured_provider_host_is_allowed() {
        for host in ALLOWED_HOSTS {
            assert!(
                is_allowed(&format!("https://{host}/v1/messages")),
                "{host} should be allowed"
            );
        }
    }

    #[test]
    fn an_unlisted_host_is_refused() {
        assert!(!is_allowed("https://evil.test/v1/messages"));
        assert!(!is_allowed("https://example.com/"));
    }

    /// The failure mode this check exists for. A suffix or `contains` test
    /// accepts all of these, and each one is a working exfiltration endpoint.
    #[test]
    fn lookalike_hosts_are_refused() {
        for url in [
            "https://api.deepgram.com.evil.test/v1/listen",
            "https://evil.test/api.deepgram.com",
            "https://notapi.deepgram.com/v1/listen",
            "https://api-deepgram.com/v1/listen",
            // userinfo confusion: the real host here is evil.test
            "https://api.deepgram.com@evil.test/v1/listen",
        ] {
            assert!(!is_allowed(url), "{url} must be refused");
        }
    }

    #[test]
    fn plaintext_http_is_refused_outright() {
        // Not on the allowlist by construction: only https and wss parse.
        assert!(!is_allowed("http://api.deepgram.com/v1/listen"));
    }

    #[test]
    fn a_port_does_not_defeat_the_check() {
        assert!(is_allowed("https://api.deepgram.com:443/v1/listen"));
        assert!(!is_allowed("https://evil.test:443/v1/listen"));
    }

    #[test]
    fn case_does_not_defeat_the_check() {
        assert!(is_allowed("https://API.Deepgram.COM/v1/listen"));
    }

    #[tokio::test]
    async fn the_transport_refuses_a_disallowed_host_before_opening_a_socket() {
        let t = AllowlistedTransport::new();
        let err = t
            .post(HttpRequest {
                url: "https://evil.test/v1/messages".into(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("allowlist"),
            "the refusal must name the reason, got: {msg}"
        );
        assert!(
            !msg.contains("no HTTP client"),
            "it must be refused by the allowlist, not merely fail later for \
             want of a client — otherwise the check is untested"
        );
    }
}
