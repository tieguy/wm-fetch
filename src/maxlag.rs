//! Action API (api.php) compliance: maxlag injection and the HTTP-200
//! maxlag error form.
//!
//! On replica lag, api.php returns **HTTP 200** with a JSON body whose
//! `error.code` is `"maxlag"`, plus `Retry-After` / `X-Database-Lag`
//! headers. A compliant client must sniff the body, not just status codes.
//! https://www.mediawiki.org/wiki/Manual:Maxlag_parameter

use url::Url;

/// Append `maxlag=N` to an Action API URL when the caller didn't include
/// one. Only fires when the URL **path** ends with `api.php`.
pub fn inject(url: &str, maxlag: u64) -> String {
    let Ok(parsed) = Url::parse(url) else {
        return url.to_string();
    };
    if !parsed.path().ends_with("api.php") {
        return url.to_string();
    }
    let already = parsed
        .query()
        .map(|q| {
            q.split('&')
                .any(|kv| kv == "maxlag" || kv.starts_with("maxlag="))
        })
        .unwrap_or(false);
    if already {
        return url.to_string();
    }
    let sep = match parsed.query() {
        Some(q) if !q.is_empty() => '&',
        Some(_) => '\u{0}', // URL already ends with '?' — replace it below.
        None => '?',
    };
    if sep == '\u{0}' {
        // `https://h/w/api.php?` — append directly, keeping one '?'.
        format!("{url}maxlag={maxlag}")
    } else {
        format!("{url}{sep}maxlag={maxlag}")
    }
}

/// A sniffed maxlag lag error inside an HTTP-200 Action API response.
#[derive(Debug, Clone)]
pub struct MaxlagError {
    pub info: Option<String>,
}

/// Detect the JSON maxlag error form. Non-JSON bodies (e.g. `format=xml`)
/// pass through untouched and are documented as not sniffed.
pub fn sniff_error(body: &[u8]) -> Option<MaxlagError> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let err = v.get("error")?;
    if err.get("code").and_then(|c| c.as_str()) != Some("maxlag") {
        return None;
    }
    Some(MaxlagError {
        info: err.get("info").and_then(|i| i.as_str()).map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_on_bare_api_url() {
        assert_eq!(
            inject(
                "https://en.wikipedia.org/w/api.php?action=query&format=json",
                5
            ),
            "https://en.wikipedia.org/w/api.php?action=query&format=json&maxlag=5"
        );
    }

    #[test]
    fn no_double_injection() {
        let u = "https://en.wikipedia.org/w/api.php?maxlag=abc";
        assert_eq!(inject(u, 5), u);
        let u2 = "https://en.wikipedia.org/w/api.php?action=query&maxlag=2";
        assert_eq!(inject(u2, 5), u2);
    }

    #[test]
    fn no_injection_on_non_api_urls() {
        let u = "https://en.wikipedia.org/wiki/Foo?action=query";
        assert_eq!(inject(u, 5), u);
    }

    #[test]
    fn no_injection_when_api_php_only_in_query() {
        let u = "https://en.wikipedia.org/wiki/Foo?ref=api.php";
        assert_eq!(inject(u, 5), u);
    }

    #[test]
    fn empty_query_edge() {
        assert_eq!(
            inject("https://en.wikipedia.org/w/api.php?", 5),
            "https://en.wikipedia.org/w/api.php?maxlag=5"
        );
    }

    #[test]
    fn sniffs_maxlag_json() {
        let body =
            br#"{"error":{"code":"maxlag","info":"Waiting for 10.0.0.1: 7 seconds lagged"}}"#;
        let e = sniff_error(body).expect("should sniff");
        assert!(e.info.as_deref().unwrap().contains("lagged"));
    }

    #[test]
    fn ignores_other_errors_and_non_json() {
        assert!(sniff_error(br#"{"error":{"code":"badinteger"}}"#).is_none());
        assert!(sniff_error(b"<api><error code=\"maxlag\"/></api>").is_none());
        assert!(sniff_error(b"plain text").is_none());
    }
}
