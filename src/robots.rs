//! Scoped robots.txt enforcement (see README "robots.txt" for the stance).
//!
//! API endpoints (`api.php`, `/api/...`, `/rest.php...`) are exempt — they
//! are governed by the API framework WMF wrote for them. Web paths honor
//! robots.txt, fail-closed per RFC 9309 when it cannot be fetched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use url::Url;

pub const ROBOT_POLICY_URL: &str = "https://wikitech.wikimedia.org/wiki/Robot_policy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    Disallowed,
}

/// Match a URL against a robots.txt body for `product` (e.g. "wm-fetch-bot").
/// An unparseable 2xx body is treated as allow-all (RFC 9309 §2.3.1).
pub fn evaluate(body: &str, product: &str, target: &Url) -> Verdict {
    match texting_robots::Robot::new(product, body.as_bytes()) {
        Ok(robot) => {
            if robot.allowed(target.as_str()) {
                Verdict::Allowed
            } else {
                Verdict::Disallowed
            }
        }
        Err(_) => Verdict::Allowed,
    }
}

pub fn crawl_delay_ms(body: &str, product: &str) -> Option<u64> {
    let robot = texting_robots::Robot::new(product, body.as_bytes()).ok()?;
    robot.delay.map(|secs| (secs * 1000.0).round() as u64)
}

fn cache_path(state_dir: &Path, host: &str) -> PathBuf {
    state_dir.join("robots").join(format!("{host}.txt"))
}

/// Fresh (≤1h) cached copy, keyed by file mtime.
pub fn cache_fresh(state_dir: &Path, host: &str) -> Option<String> {
    let path = cache_path(state_dir, host);
    let meta = std::fs::metadata(&path).ok()?;
    let age_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    if super::pacing::now_ms().saturating_sub(age_ms) > super::pacing::ROBOTS_CACHE_TTL_MS {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Any-age cached copy — the fallback when a robots.txt fetch fails
/// (fail-closed still prefers a stale rule set over no rule set).
pub fn cache_any_age(state_dir: &Path, host: &str) -> Option<String> {
    std::fs::read_to_string(cache_path(state_dir, host)).ok()
}

pub fn write_cache(state_dir: &Path, host: &str, body: &str) {
    let _ = std::fs::create_dir_all(state_dir.join("robots"));
    let _ = std::fs::write(cache_path(state_dir, host), body);
}

/// Per-invocation registry of learned crawl-delays (host → delay).
#[derive(Debug, Default)]
pub struct CrawlDelays(HashMap<String, u64>);

impl CrawlDelays {
    pub fn set(&mut self, host: &str, delay_ms: u64) {
        self.0.insert(host.to_string(), delay_ms);
    }
    pub fn get(&self, host: &str) -> Option<u64> {
        self.0.get(host).copied()
    }
}

pub fn robots_url_for(target: &Url) -> Url {
    let mut u = target.clone();
    u.set_path("/robots.txt");
    u.set_query(None);
    u.set_fragment(None);
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIKIMEDIA_ROBOTS: &str = "User-agent: *\nAllow: /w/api.php?action=mobileview&\nAllow: /w/load.php?\nAllow: /api/rest_v1/?doc\nDisallow: /w/\nDisallow: /api/\nDisallow: /trap/\nDisallow: /wiki/Special:\nDisallow: /wiki/Special%3A\n";

    #[test]
    fn evaluate_wikimedia_rules() {
        let ev = |path: &str| {
            evaluate(
                WIKIMEDIA_ROBOTS,
                "wm-fetch-bot",
                &Url::parse(&format!("https://en.wikipedia.org{path}")).unwrap(),
            )
        };
        assert_eq!(ev("/wiki/Foo"), Verdict::Allowed);
        assert_eq!(ev("/wiki/Special:Export"), Verdict::Disallowed);
        assert_eq!(ev("/wiki/Special%3AExport"), Verdict::Disallowed);
        assert_eq!(ev("/trap/x"), Verdict::Disallowed);
        assert_eq!(ev("/w/index.php?title=Foo"), Verdict::Disallowed); // Disallow: /w/
    }

    #[test]
    fn crawl_delay_parsing() {
        assert_eq!(crawl_delay_ms(WIKIMEDIA_ROBOTS, "wm-fetch-bot"), None);
        let with_delay = "User-agent: *\nCrawl-delay: 2\nDisallow: /x\n";
        assert_eq!(crawl_delay_ms(with_delay, "wm-fetch-bot"), Some(2000));
    }

    #[test]
    fn garbage_robots_is_allow_all() {
        let v = evaluate(
            "complete garbage {{{{",
            "wm-fetch-bot",
            &Url::parse("https://h/x").unwrap(),
        );
        assert_eq!(v, Verdict::Allowed);
    }

    #[test]
    fn cache_roundtrip_and_ttl() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cache_fresh(dir.path(), "h").is_none());
        write_cache(dir.path(), "h", "User-agent: *\nDisallow: \n");
        assert_eq!(
            cache_fresh(dir.path(), "h").unwrap(),
            "User-agent: *\nDisallow: \n"
        );
        assert_eq!(
            cache_any_age(dir.path(), "h").unwrap(),
            "User-agent: *\nDisallow: \n"
        );
    }
}
