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

/// A robots.txt verdict as recorded on a result: the verdict itself plus
/// which rules matched. For record-only consumers
/// (`SessionOptions::robots_mode`); `NoRobots` means robots.txt could not
/// be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportVerdict {
    Allowed,
    Disallowed,
    NoRobots,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub verdict: ReportVerdict,
    /// The matching rule lines, longest first (e.g.
    /// `disallow: /wiki/Special:`). Empty for 4xx robots.txt (allow-all)
    /// and unreachability.
    pub matched_rules: Vec<String>,
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

/// Approximate extraction of the robots.txt rules that matched `target`
/// for `product`, for recording alongside a verdict. The verdict itself
/// always comes from [`evaluate`] (texting_robots); this helper re-parses
/// the common rule forms (exact prefix with `*` and `$` wildcards) and may
/// miss exotic ones. Rule lines are returned longest-first.
pub fn matched_rules(body: &str, product: &str, target: &Url) -> Vec<String> {
    let groups = parse_groups(body);
    let Some(rules) = select_group(&groups, product) else {
        return Vec::new();
    };
    let mut path_q = percent_decode(target.path());
    if let Some(q) = target.query() {
        path_q.push('?');
        path_q.push_str(&percent_decode(q));
    }
    let mut matched: Vec<String> = rules
        .iter()
        .filter(|(kind, rule)| {
            (kind == "allow" || kind == "disallow") && rule_matches(rule, &path_q)
        })
        .map(|(kind, rule)| format!("{kind}: {rule}"))
        .collect();
    matched.sort_by_key(|s| std::cmp::Reverse(s.len()));
    matched
}

/// One robots.txt group: the user-agent tokens it was declared for, plus
/// its rule lines as `(kind, value)` pairs.
struct Group {
    agents: Vec<String>,
    rules: Vec<(String, String)>,
}

fn parse_groups(body: &str) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut prev_was_agent = false;
    for raw in body.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            prev_was_agent = false;
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        match key.as_str() {
            "user-agent" => {
                if !prev_was_agent {
                    groups.push(Group {
                        agents: Vec::new(),
                        rules: Vec::new(),
                    });
                }
                groups
                    .last_mut()
                    .expect("group just pushed")
                    .agents
                    .push(value.to_ascii_lowercase());
                prev_was_agent = true;
            }
            "allow" | "disallow" | "crawl-delay" => {
                if let Some(group) = groups.last_mut() {
                    group.rules.push((key, value));
                }
                prev_was_agent = false;
            }
            _ => prev_was_agent = false,
        }
    }
    groups
}

/// Spec behaviour: the group matching the product token exactly wins; if
/// none, the `*` group applies.
fn select_group<'a>(groups: &'a [Group], product: &str) -> Option<&'a [(String, String)]> {
    let product = product.to_ascii_lowercase();
    let wildcard = groups.iter().find(|g| g.agents.iter().any(|a| a == "*"));
    let chosen = groups
        .iter()
        .find(|g| g.agents.contains(&product))
        .or(wildcard)?;
    Some(&chosen.rules)
}

/// Robots rule matching: pattern anchored at the start of the target,
/// `*` matches any run of characters, a trailing `$` requires the match to
/// reach the end of the target.
fn rule_matches(pattern: &str, target: &str) -> bool {
    let mut pat: Vec<char> = pattern.chars().collect();
    let mut anchored_end = false;
    if pat.last() == Some(&'$') {
        pat.pop();
        anchored_end = true;
    }
    let text: Vec<char> = target.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    loop {
        if p == pat.len() {
            if !anchored_end || t == text.len() {
                return true;
            }
        } else if pat[p] == '*' {
            star = p;
            mark = t;
            p += 1;
            continue;
        } else if t < text.len() && pat[p] == text[t] {
            p += 1;
            t += 1;
            continue;
        }
        // Backtrack: let the last `*` absorb one more character — only
        // while characters remain. Without the bound, a wildcard pattern
        // that does not match walks `mark` past the end of the target and
        // loops forever.
        if star != usize::MAX && mark < text.len() {
            p = star + 1;
            mark += 1;
            t = mark;
            continue;
        }
        return false;
    }
}

/// Minimal percent-decoding for rule comparison (Wikimedia disallows
/// `Special:` but URLs arrive percent-encoded as `Special%3A`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| {
                std::str::from_utf8(h)
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            });
            match hex {
                Some(b) => {
                    out.push(b);
                    i += 3;
                }
                None => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
    fn matched_rules_extraction() {
        let u = |p: &str| Url::parse(&format!("https://en.wikipedia.org{p}")).unwrap();
        // Wildcard group, plain prefix.
        assert_eq!(
            matched_rules(WIKIMEDIA_ROBOTS, "wm-fetch-bot", &u("/wiki/Special:Export")),
            vec!["disallow: /wiki/Special:".to_string()]
        );
        // Percent-encoded target decodes for comparison.
        assert_eq!(
            matched_rules(
                WIKIMEDIA_ROBOTS,
                "wm-fetch-bot",
                &u("/wiki/Special%3AExport")
            ),
            vec!["disallow: /wiki/Special:".to_string()]
        );
        // Allowed path: the disallow rules do not match → no rules.
        assert!(matched_rules(WIKIMEDIA_ROBOTS, "wm-fetch-bot", &u("/wiki/Foo")).is_empty());

        // Wildcards and the exact-group preference.
        let body = "User-agent: *\nDisallow: /a\n\nUser-agent: wm-fetch-bot\nDisallow: /b/*\n";
        assert_eq!(
            matched_rules(body, "wm-fetch-bot", &u("/b/x")),
            vec!["disallow: /b/*".to_string()]
        );
        assert_eq!(
            matched_rules(body, "wm-fetch-bot", &u("/a")),
            Vec::<String>::new()
        );
        assert_eq!(
            matched_rules(body, "other-bot", &u("/a")),
            vec!["disallow: /a".to_string()]
        );

        // `$` anchor: /foo$ does not match /foobar.
        let anchored = "User-agent: *\nDisallow: /foo$\n";
        assert_eq!(
            matched_rules(anchored, "wm-fetch-bot", &u("/foo")),
            vec!["disallow: /foo$".to_string()]
        );
        assert!(matched_rules(anchored, "wm-fetch-bot", &u("/foobar")).is_empty());
    }

    #[test]
    fn rule_matching_basics() {
        use super::rule_matches;
        assert!(rule_matches("/wiki/", "/wiki/Foo"));
        assert!(!rule_matches("/wiki/", "/w/Foo"));
        assert!(rule_matches("", "/anything"));
        assert!(rule_matches("/*/x", "/a/b/x"));
        assert!(rule_matches("/foo$", "/foo"));
        assert!(!rule_matches("/foo$", "/foobar"));
    }

    /// Wildcard patterns that do NOT match must return false promptly
    /// (they used to loop forever). Rules from a real WordPress
    /// robots.txt (bernews.com) against one of its article paths. Run on
    /// a thread with a deadline so a regression fails instead of hanging
    /// the suite.
    #[test]
    fn non_matching_wildcards_terminate() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use super::rule_matches;
            let target = "/2025/08/aug15-kalen-brunson-makes-qpr-debut-cup/";
            let results = [
                rule_matches("/*trackback/", target),
                rule_matches("/2*feed/", target),
                rule_matches("/*?s", target),
                rule_matches("/*.pdf$", target),
                rule_matches("*x", ""),
                rule_matches("/a*b", "/a"),
                // Wildcards that do match, including with `$` anchors.
                rule_matches("/*cup/", target),
                rule_matches("/a*$", "/a"),
                rule_matches("/a*$", "/abc"),
                rule_matches("/*.pdf$", "/x/y.pdf"),
                rule_matches("/*.pdf$", "/x.pdf.pdf"),
            ];
            let _ = tx.send(results);
        });
        let results = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("rule_matches must terminate on non-matching wildcards");
        assert_eq!(
            results,
            [false, false, false, false, false, false, true, true, true, true, true]
        );
    }

    #[test]
    fn evaluate_terminates_on_wordpress_wildcards() {
        let body = "User-agent: *\nDisallow: /wp-admin/\nDisallow: /*?replytocom\n\
                    Disallow: /*trackback/\nDisallow: /2*feed/\nDisallow: /*?s\n\
                    Crawl-delay: 3\n";
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let target = Url::parse("https://example.org/2025/08/some-article/").unwrap();
            let _ = tx.send((
                evaluate(body, "citation-fetcher", &target),
                matched_rules(body, "citation-fetcher", &target),
            ));
        });
        let (verdict, rules) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("evaluate must terminate");
        assert_eq!(verdict, Verdict::Allowed);
        assert!(rules.is_empty(), "{rules:?}");
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(super::percent_decode("Special%3AExport"), "Special:Export");
        assert_eq!(super::percent_decode("%zz"), "%zz");
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
