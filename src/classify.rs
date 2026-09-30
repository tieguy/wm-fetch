//! URL surface classification and host class — pure logic, heavily unit-tested.
//!
//! Classification is on the URL **path only**. v1's whole-URL `*api.php*`
//! glob misfired on query strings such as `/wiki/Foo?ref=api.php`.

use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// Action API: path ends with `api.php` (e.g. `/w/api.php`).
    ActionApi,
    /// REST surfaces: `/api/...` (rest_v1 etc.) or any `/rest.php` path.
    RestApi,
    /// Everything else (e.g. `/wiki/Foo`, `/w/index.php`) — subject to robots.txt.
    Web,
}

pub fn surface(url: &Url) -> Surface {
    let path = url.path();
    if path.ends_with("api.php") {
        Surface::ActionApi
    } else if path.starts_with("/api/") || path.contains("/rest.php") {
        Surface::RestApi
    } else {
        Surface::Web
    }
}

/// API endpoints are governed by the API framework WMF actually wrote for
/// them (UA policy + API:Etiquette + Robot policy API rules), not by the
/// crawler-oriented robots.txt `Disallow: /w/` / `Disallow: /api/` lines.
/// See README "robots.txt" for the full reasoning.
pub fn is_api_exempt(url: &Url) -> bool {
    surface(url) != Surface::Web
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    /// Wiki content/API hosts and all non-Wikimedia hosts: ≥250ms floor.
    Default,
    /// gerrit/gitlab/phabricator/lists under *.wikimedia.org:
    /// ≥1s floor and a 15-minute pause after any 5xx (Wikitech Robot policy,
    /// "other wikimedia.org services" row).
    OtherServices,
}

/// Label-anchored under `*.wikimedia.org`: the hostname must be exactly one
/// of `gerrit.` / `gitlab.` / `phabricator.` / `lists.` + `wikimedia.org`.
/// Never a raw substring match — `lists.example.org` must NOT match.
pub fn host_class(host: &str) -> HostClass {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() == 3
        && matches!(labels[0], "gerrit" | "gitlab" | "phabricator" | "lists")
        && labels[1] == "wikimedia"
        && labels[2] == "org"
    {
        HostClass::OtherServices
    } else {
        HostClass::Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn path_classification() {
        assert_eq!(
            surface(&u("https://en.wikipedia.org/w/api.php?action=query")),
            Surface::ActionApi
        );
        assert_eq!(
            surface(&u("https://en.wikipedia.org/w/rest.php/v1/page/Foo")),
            Surface::RestApi
        );
        assert_eq!(
            surface(&u("https://en.wikipedia.org/api/rest_v1/page/summary/Foo")),
            Surface::RestApi
        );
        assert_eq!(
            surface(&u("https://en.wikipedia.org/wiki/Foo")),
            Surface::Web
        );
        assert_eq!(
            surface(&u("https://en.wikipedia.org/w/index.php?title=Foo")),
            Surface::Web
        );
        assert_eq!(
            surface(&u("https://en.wikipedia.org/wiki/Special:Export")),
            Surface::Web
        );
    }

    #[test]
    fn path_only_never_query() {
        // v1 false positive: whole-URL substring matched the query string.
        assert_eq!(
            surface(&u("https://en.wikipedia.org/wiki/Foo?ref=api.php")),
            Surface::Web
        );
        assert!(!is_api_exempt(&u(
            "https://en.wikipedia.org/wiki/Foo?ref=api.php"
        )));
    }

    #[test]
    fn api_exempt_classification() {
        assert!(is_api_exempt(&u("https://en.wikipedia.org/w/api.php")));
        assert!(is_api_exempt(&u(
            "https://en.wikipedia.org/w/api.php?maxlag=abc"
        )));
        assert!(is_api_exempt(&u("https://www.mediawiki.org/w/api.php")));
        assert!(!is_api_exempt(&u("https://en.wikipedia.org/wiki/Foo")));
    }

    #[test]
    fn host_class_labels() {
        for h in [
            "gerrit.wikimedia.org",
            "gitlab.wikimedia.org",
            "phabricator.wikimedia.org",
            "lists.wikimedia.org",
        ] {
            assert_eq!(host_class(h), HostClass::OtherServices, "{h}");
        }
        for h in [
            "en.wikipedia.org",
            "meta.wikimedia.org",
            "www.mediawiki.org",
            "example.org",
            "lists.example.org",
            "gerrit.example.com",
            "mygerrit.wikimedia.org",
            "wikimedia.org",
            "gerrit.wikimedia.org.example.net",
        ] {
            assert_eq!(host_class(h), HostClass::Default, "{h}");
        }
        // Case/trailing-dot tolerance.
        assert_eq!(
            host_class("GERRIT.Wikimedia.org."),
            HostClass::OtherServices
        );
    }
}
