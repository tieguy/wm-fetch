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
    /// Wikimedia project hosts (wikipedia.org, wikidata.org, …): the
    /// machine-wide serialization and global pacing state apply unchanged.
    Wikimedia,
    /// gerrit/gitlab/phabricator/lists under *.wikimedia.org:
    /// ≥1s floor and a 15-minute pause after any 5xx (Wikitech Robot policy,
    /// "other wikimedia.org services" row). Same lock bucket as
    /// Wikimedia.
    OtherServices,
    /// Everything else: per-host lock and per-host pacing state; the
    /// machine-wide concurrency is capped by the slot pool.
    Default,
}

/// The WMF domain family. Suffix matching is label-anchored
/// (`en.wikipedia.org` matches; `notwikipedia.org` does not). Over-broad
/// is the safe direction: a host kept in the global bucket merely stays
/// machine-wide serialized, exactly as before 2.1.
const WIKIMEDIA_DOMAINS: &[&str] = &[
    "wikipedia.org",
    "wikimedia.org",
    "wikidata.org",
    "wikibooks.org",
    "wiktionary.org",
    "wikiquote.org",
    "wikisource.org",
    "wikinews.org",
    "wikiversity.org",
    "wikivoyage.org",
    "mediawiki.org",
    "wikifunctions.org",
    "wikimediafoundation.org",
    "toolforge.org",
    "wmcloud.org",
    "wmflabs.org",
];

/// Whether a host belongs to the WMF domain family.
#[must_use]
pub fn is_wikimedia_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    WIKIMEDIA_DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Test-only tighten override: when `WM_FETCH_WMF_TEST_FORCE_WIKIMEDIA=1`,
/// every host classifies as Wikimedia, so offline loopback tests exercise
/// the global-lock bucket. Tighten-only — there is no override in the
/// relaxing direction, so it cannot weaken compliance.
fn forced_wikimedia() -> bool {
    std::env::var("WM_FETCH_WMF_TEST_FORCE_WIKIMEDIA").as_deref() == Ok("1")
}

/// Any 5xx from an other-services host arms the 15-minute cooldown (and is
/// never retried). Pure so the policy decision is unit-tested directly.
pub fn cooldown_triggered(status: u16, class: HostClass) -> bool {
    class == HostClass::OtherServices && status >= 500
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
        return HostClass::OtherServices;
    }
    if forced_wikimedia() || is_wikimedia_host(&host) {
        return HostClass::Wikimedia;
    }
    HostClass::Default
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
            "www.wikidata.org",
            "en.wiktionary.org",
            "citation-fetcher.toolforge.org",
        ] {
            assert_eq!(host_class(h), HostClass::Wikimedia, "{h}");
        }
        for h in [
            "example.org",
            "lists.example.org",
            "gerrit.example.com",
            "wikimedia.org.example.net",
            "notwikipedia.org",
            "web.archive.org",
        ] {
            assert_eq!(host_class(h), HostClass::Default, "{h}");
        }
        // Case/trailing-dot tolerance.
        assert_eq!(
            host_class("GERRIT.Wikimedia.org."),
            HostClass::OtherServices
        );
        assert_eq!(host_class("EN.Wikipedia.org."), HostClass::Wikimedia);
    }

    #[test]
    fn wikimedia_domain_family() {
        assert!(is_wikimedia_host("wikipedia.org"));
        assert!(is_wikimedia_host("de.wikipedia.org"));
        assert!(!is_wikimedia_host("wikipedia.org.evil.net"));
        assert!(!is_wikimedia_host("wikipedia.example.org"));
    }

    #[test]
    fn cooldown_policy() {
        // Any 5xx from an other-services host arms the cooldown; nothing
        // else does.
        for status in [500u16, 502, 503, 504] {
            assert!(
                cooldown_triggered(status, HostClass::OtherServices),
                "{status}"
            );
            assert!(!cooldown_triggered(status, HostClass::Default), "{status}");
        }
        for status in [200u16, 301, 404, 429] {
            assert!(
                !cooldown_triggered(status, HostClass::OtherServices),
                "{status}"
            );
        }
    }
}
