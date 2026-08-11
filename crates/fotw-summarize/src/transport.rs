//! The HTTP seam (spec 5.6: "the pipeline must be provable with no secrets").
//!
//! This crate contains **no HTTP client and no TLS**. The provider adapter
//! builds a request body and hands it to whatever [`HttpTransport`] the daemon
//! injected. Three things fall out of that, and all three are the point:
//!
//! 1. **CI can assert on request construction.** "Citations and
//!    `output_config.format` are never in the same body" (spec 8.4) is a claim
//!    about bytes on the wire, so it has to be tested against the bytes, and a
//!    recorded transport is the only way to see them without a key.
//! 2. **No TLS stack enters this crate's dependency graph.** `deny.toml`'s
//!    allowlist has no `OpenSSL` entry, which is why `fotw-stt` documents its
//!    `native-tls` choice at its own call site; not having the dependency at
//!    all is strictly better than having the right one.
//! 3. **The trait is dyn-compatible.** Hence the boxed future rather than
//!    `async fn` in trait: the daemon stores one `Arc<dyn HttpTransport>` and
//!    swaps it for a proxy or a recorder without generics reaching the config
//!    layer.

use std::future::Future;
use std::pin::Pin;

use crate::error::SummarizeError;

/// A boxed, `Send` future. The dyn-compatible spelling of `async fn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A JSON POST, fully materialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Absolute URL.
    pub url: String,
    /// Header name/value pairs. The adapter sets `content-type`, the API
    /// version and the auth header; the transport adds nothing of its own.
    pub headers: Vec<(String, String)>,
    /// The request body.
    pub body: Vec<u8>,
}

/// A response, fully buffered.
///
/// Buffered rather than streamed because the two-call pipeline (spec 8.4) is
/// batch-shaped. SUM-10's streaming requirement is a separate seam that will
/// take a callback; deliberately not modelled here so that this trait stays
/// trivial to implement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// The response body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The body as UTF-8, lossily. Only for error messages.
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Whatever actually moves bytes to the provider.
pub trait HttpTransport: Send + Sync {
    /// POST `request` and return the whole response.
    ///
    /// Implementations map connection-level failures to
    /// [`SummarizeError::Transport`] and must **not** map non-2xx statuses to
    /// an error — the adapter needs the status and the body to distinguish a
    /// rate limit from a schema rejection.
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, SummarizeError>>;
}

impl<T: HttpTransport + ?Sized> HttpTransport for std::sync::Arc<T> {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, SummarizeError>> {
        (**self).post(request)
    }
}
