//! Cross-invocation pacing (Wikitech Robot policy) and the other-services
//! cooldown. Policy-derived floors are constants — never configurable.
//!
//! Floors (strictest applicable subset, applied globally):
//! - Default hosts: ≥250ms between request ends (≤4 req/s, strictly
//!   "below 5 req/s"), ≥5000ms after a request that took >1s to serve
//!   (Action API slow-pause rule).
//! - Other-services (gerrit/gitlab/phabricator/lists): ≥1000ms between
//!   request ends, and a 15-minute pause after any 5xx from that host.
//! - robots.txt Crawl-delay, when declared: effective floor is
//!   max(crawl-delay, host floor).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::classify::HostClass;

pub const DEFAULT_FLOOR_MS: u64 = 250;
pub const OTHER_SERVICES_FLOOR_MS: u64 = 1000;
pub const EXPENSIVE_THRESHOLD_MS: u64 = 1000;
pub const EXPENSIVE_PAUSE_MS: u64 = 5000;
pub const COOLDOWN_MS: u64 = 15 * 60 * 1000;
pub const ROBOTS_CACHE_TTL_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RequestState {
    pub last_request_end_ms: Option<u64>,
    pub last_request_duration_ms: Option<u64>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Pure pacer math: how long must we wait before the next request to a host
/// of `class`, given the recorded previous request and any declared
/// robots.txt Crawl-delay?
pub fn required_wait_ms(
    state: &RequestState,
    now: u64,
    class: HostClass,
    crawl_delay_ms: Option<u64>,
) -> u64 {
    let Some(end) = state.last_request_end_ms else {
        return 0;
    };
    let mut gap = match class {
        HostClass::OtherServices => OTHER_SERVICES_FLOOR_MS,
        HostClass::Default => DEFAULT_FLOOR_MS,
    };
    if let Some(cd) = crawl_delay_ms {
        gap = gap.max(cd);
    }
    if state.last_request_duration_ms.unwrap_or(0) > EXPENSIVE_THRESHOLD_MS {
        gap = gap.max(EXPENSIVE_PAUSE_MS);
    }
    gap.saturating_sub(now.saturating_sub(end))
}

pub fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

/// Corrupt or missing state reads as "no previous request" — pacing simply
/// doesn't fire. Recorded state is an optimization for politeness, not a
/// correctness gate.
pub fn load_state(dir: &Path) -> RequestState {
    fs::read_to_string(state_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Record one completed outbound request (any kind — target, robots.txt,
/// redirect hop): its end time and how long it took to serve.
pub fn record_request(dir: &Path, started: Instant) {
    let duration_ms = started.elapsed().as_millis() as u64;
    let st = RequestState {
        last_request_end_ms: Some(now_ms()),
        last_request_duration_ms: Some(duration_ms),
    };
    let _ = fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_string(&st) {
        let tmp = state_path(dir).with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, state_path(dir));
        }
    }
}

fn cooldown_file(dir: &Path, host: &str) -> PathBuf {
    dir.join("cooldown").join(format!("{host}.until"))
}

/// If an other-services host is in its post-5xx cooldown, returns the
/// epoch-ms until which we must not touch it.
pub fn cooldown_until(dir: &Path, host: &str) -> Option<u64> {
    let until: u64 = fs::read_to_string(cooldown_file(dir, host))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (until > now_ms()).then_some(until)
}

pub fn record_cooldown(dir: &Path, host: &str) -> u64 {
    let until = now_ms() + COOLDOWN_MS;
    let _ = fs::create_dir_all(dir.join("cooldown"));
    let _ = fs::write(cooldown_file(dir, host), until.to_string());
    until
}

/// Format epoch-ms as UTC HH:MM for cooldown messages.
pub fn fmt_hhmm(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // Civil-from-days (Howard Hinnant's algorithm) for a human date.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn st(end: u64, dur: u64) -> RequestState {
        RequestState {
            last_request_end_ms: Some(end),
            last_request_duration_ms: Some(dur),
        }
    }

    #[test]
    fn pacer_math_basic_floor() {
        // Default class, cheap previous request, 100ms elapsed → wait 150ms.
        assert_eq!(
            required_wait_ms(&st(1000, 100), 1100, HostClass::Default, None),
            150
        );
        // Fully elapsed → no wait.
        assert_eq!(
            required_wait_ms(&st(1000, 100), 1300, HostClass::Default, None),
            0
        );
        // No previous request → no wait.
        assert_eq!(
            required_wait_ms(&RequestState::default(), 0, HostClass::Default, None),
            0
        );
    }

    #[test]
    fn pacer_math_expensive_pause() {
        // Previous request took >1s → 5s gap required.
        assert_eq!(
            required_wait_ms(&st(1000, 1500), 2000, HostClass::Default, None),
            4000
        );
        // Exactly at the 5s boundary → 0.
        assert_eq!(
            required_wait_ms(&st(1000, 1500), 6000, HostClass::Default, None),
            0
        );
        // 1000ms duration is NOT >1000ms → normal floor applies.
        assert_eq!(
            required_wait_ms(&st(1000, 1000), 1100, HostClass::Default, None),
            150
        );
    }

    #[test]
    fn pacer_math_other_services() {
        assert_eq!(
            required_wait_ms(&st(1000, 100), 1100, HostClass::OtherServices, None),
            900
        );
    }

    #[test]
    fn pacer_math_crawl_delay_vs_floor() {
        // Crawl-delay larger than floor wins.
        assert_eq!(
            required_wait_ms(&st(1000, 100), 1100, HostClass::Default, Some(2000)),
            1900
        );
        // Smaller than floor loses.
        assert_eq!(
            required_wait_ms(&st(1000, 100), 1100, HostClass::Default, Some(100)),
            150
        );
        // But never shrinks the post-expensive pause.
        assert_eq!(
            required_wait_ms(&st(1000, 1500), 2000, HostClass::Default, Some(100)),
            4000
        );
    }

    #[test]
    fn cooldown_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cooldown_until(dir.path(), "gerrit.wikimedia.org").is_none());
        let until = record_cooldown(dir.path(), "gerrit.wikimedia.org");
        assert!(until > now_ms());
        assert_eq!(
            cooldown_until(dir.path(), "gerrit.wikimedia.org"),
            Some(until)
        );
    }

    #[test]
    fn state_roundtrip_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_state(dir.path()).last_request_end_ms, None);
        record_request(dir.path(), Instant::now() - Duration::from_millis(1200));
        let st = load_state(dir.path());
        assert!(st.last_request_duration_ms.unwrap() >= 1200);
        assert!(st.last_request_end_ms.unwrap() > 0);
        // Corrupt file → treated as no state, not an error.
        fs::write(state_path(dir.path()), "not json").unwrap();
        assert_eq!(load_state(dir.path()).last_request_end_ms, None);
    }

    #[test]
    fn hhmm_format() {
        assert_eq!(fmt_hhmm(0), "1970-01-01 00:00Z");
        // Known instant: 1700000000 = 2023-11-14 22:13:20 UTC.
        assert_eq!(fmt_hhmm(1_700_000_000_000), "2023-11-14 22:13Z");
    }
}
