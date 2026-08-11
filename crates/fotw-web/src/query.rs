//! A query-string reader, written out rather than extracted.
//!
//! `axum::extract::Query` would do this, and its **rejection** is the problem:
//! it renders as a `400` whose body reads `Failed to deserialize query string:
//! missing field 'q'`. That body is an oracle. It tells an unauthenticated
//! caller that the path exists, that it takes a query string, and what the
//! field is called — which is three facts more than ING-09's uniform bare 404
//! is willing to give away. Extractor rejections are the most common way a
//! carefully uniform error surface springs a leak, because they are written by
//! a crate that is optimising for a developer's debugging experience and has
//! never heard of this threat model.
//!
//! So: no extractor, no rejection, no serde. Two functions, both total.

/// The first value of `key`, percent-decoded, or `None` if the key is absent.
///
/// First rather than last, and no support for repeated keys: `?q=a&q=b` is
/// ambiguous, every framework resolves it differently, and picking a rule
/// explicitly is how a parameter-pollution bug stays out.
#[must_use]
pub fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        (percent_decode(k) == key).then(|| percent_decode(v))
    })
}

/// `application/x-www-form-urlencoded` decoding: `+` is a space, `%XX` is a
/// byte.
///
/// Invalid escapes are left alone rather than rejected. The decoded value is
/// only ever a search term or a hex ticket — one goes to FTS5 through
/// `fotw_store`'s own tokeniser, the other to a constant-time comparison that
/// will simply fail — so there is nothing here for a malformed escape to
/// exploit, and a hard error would be a way to tell inputs apart.
#[must_use]
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // Lossy: a percent escape can encode any byte, including one that is not
    // valid UTF-8, and a search box is not worth a panic.
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_parameter() {
        assert_eq!(query_param("q=hello", "q").as_deref(), Some("hello"));
        assert_eq!(
            query_param("ticket=abc&q=hi", "ticket").as_deref(),
            Some("abc")
        );
        assert_eq!(
            query_param("q=hi&ticket=abc", "ticket").as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn an_absent_parameter_is_none() {
        assert_eq!(query_param("", "q"), None);
        assert_eq!(query_param("other=1", "q"), None);
        // A prefix match is not a match; `?qq=` must not answer for `q`.
        assert_eq!(query_param("qq=1", "q"), None);
    }

    #[test]
    fn an_empty_value_is_empty_not_absent() {
        assert_eq!(query_param("q=", "q").as_deref(), Some(""));
        assert_eq!(query_param("q", "q").as_deref(), Some(""));
    }

    #[test]
    fn decodes_percent_escapes_and_plus() {
        assert_eq!(
            query_param("q=quarterly+review", "q").as_deref(),
            Some("quarterly review")
        );
        assert_eq!(
            query_param("q=budget%20%26%20hiring", "q").as_deref(),
            Some("budget & hiring")
        );
        // A `&` that arrives encoded must not split the pair.
        assert_eq!(query_param("q=a%26b=c", "q").as_deref(), Some("a&b=c"));
        assert_eq!(query_param("q=%E2%9C%93", "q").as_deref(), Some("✓"));
    }

    #[test]
    fn a_malformed_escape_is_data_not_an_error() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn the_first_of_a_repeated_key_wins() {
        assert_eq!(query_param("q=a&q=b", "q").as_deref(), Some("a"));
    }
}
