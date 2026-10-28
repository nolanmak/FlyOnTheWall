use super::{super::catalog::DownloadSpec, Response, Transport};
use std::time::Duration;

pub(super) struct Https {
    client: reqwest::blocking::Client,
}
impl Https {
    pub(super) fn new() -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .https_only(true)
            .referer(false)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let u = attempt.url();
                let host = u.host_str().unwrap_or("");
                let allowed = host == "github.com"
                    || host == "release-assets.githubusercontent.com"
                    || host == "objects.githubusercontent.com"
                    || host == "huggingface.co"
                    || host.ends_with(".huggingface.co")
                    || host.ends_with(".hf.co");
                if attempt.previous().len() >= 5
                    || u.scheme() != "https"
                    || !allowed
                    || u.port().is_some_and(|p| p != 443)
                    || !u.username().is_empty()
                    || u.password().is_some()
                {
                    attempt.error("Artifact redirect refused")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|_| "Cannot initialize public artifact transport")?;
        Ok(Self { client })
    }
}
impl Transport for Https {
    fn open(&mut self, spec: &DownloadSpec, offset: u64) -> Result<Response, String> {
        let mut request = self
            .client
            .get(spec.url())
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        if offset > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        let response = request
            .send()
            .map_err(|_| "Artifact request failed or stalled; retained bytes can be retried")?;
        if response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return Err("Artifact encoding prevents byte-accurate verification".into());
        }
        let range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range);
        Ok(Response {
            status: response.status().as_u16(),
            length: response.content_length(),
            range,
            body: Box::new(response),
        })
    }
}
fn parse_range(value: &str) -> Option<(u64, u64, u64)> {
    let (span, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_range_preserves_large_offsets_and_rejects_ambiguous_values() {
        assert_eq!(
            parse_range("bytes 137438953472-142000000000/142000000001"),
            Some((137438953472, 142000000000, 142000000001))
        );
        for value in [
            "bytes */100",
            "items 0-99/100",
            "bytes 0-99/*",
            "bytes -1-99/100",
            "bytes 0-99/18446744073709551616",
        ] {
            assert!(parse_range(value).is_none(), "{value}");
        }
    }
}
