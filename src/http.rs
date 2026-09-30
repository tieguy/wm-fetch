//! HTTP session: paced, locked, robots-checked, manually-redirected
//! fetching. Every outbound request — target, robots.txt, redirect hops —
//! goes through the same pacing/lock/state machinery. No request kind is
//! exempt.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use url::Url;

use crate::classify::{cooldown_triggered, host_class, is_api_exempt, surface, HostClass, Surface};
use crate::config::Config;
use crate::maxlag;
use crate::pacing::{self, fmt_hhmm};
use crate::robots::{self, Verdict};

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_POLICY: i32 = 3;

/// Errors that end the invocation.
#[derive(Debug)]
pub enum Fail {
    /// Exit 1: transport failure, HTTP error after retries.
    Fatal(String),
    /// Exit 1: the wall-clock --max-time budget could not cover a required
    /// wait (pacing, lock, backoff). Distinct from Fatal so robots-fetch
    /// error handling does not swallow it into "robots.txt unreachable".
    Budget(String),
    /// Exit 3: policy refusal — robots.txt disallow, robots.txt unreachable
    /// after cache fallback, or other-services 5xx cooldown.
    Policy(String),
}

pub struct Final {
    pub status: u16,
    pub body: Vec<u8>,
    /// True when the response is being returned only because retries were
    /// exhausted (e.g. a persistent HTTP-200 maxlag error): body still goes
    /// to stdout, but the invocation is a failure (exit 1).
    pub failure: bool,
}

pub struct Session {
    cfg: Config,
    ua: String,
    state_dir: PathBuf,
    deadline: Instant,
    client: reqwest::blocking::Client,
    crawl_delays: robots::CrawlDelays,
    /// Held for the whole invocation ⇒ concurrent wm-fetch processes
    /// serialize (Robot policy: concurrency 1). Blocking bounded by the
    /// --max-time budget.
    _lock: fs::File,
}

enum RobotsFetch {
    Body(String),
    /// HTTP 4xx — RFC 9309: allow-all.
    NotFound,
    /// Network error / 5xx / too many redirects — fail-closed path.
    Unreachable,
}

impl Session {
    pub fn new(cfg: Config, ua: String, state_dir: PathBuf) -> Result<Self, Fail> {
        let connect_timeout = Duration::from_secs_f64(cfg.connect_timeout_secs.max(0.1));
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none()) // hops are driven manually
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|e| Fail::Fatal(format!("building HTTP client: {e}")))?;

        let deadline = Instant::now() + Duration::from_secs_f64(cfg.max_time_secs.max(0.1));

        fs::create_dir_all(&state_dir)
            .map_err(|e| Fail::Fatal(format!("creating state dir {}: {e}", state_dir.display())))?;
        let lock_path = state_dir.join("state.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| Fail::Fatal(format!("opening {}: {e}", lock_path.display())))?;
        use fs4::fs_std::FileExt;
        loop {
            if matches!(lock.try_lock_exclusive(), Ok(true)) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Fail::Budget(
                    "lock wait would exceed --max-time (another wm-fetch is running)".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        Ok(Self {
            cfg,
            ua,
            state_dir,
            deadline,
            client,
            crawl_delays: robots::CrawlDelays::default(),
            _lock: lock,
        })
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn budget_check(&self, wait: Duration, what: &str) -> Result<(), Fail> {
        if wait > self.remaining() {
            return Err(Fail::Budget(format!(
                "{what} would exceed --max-time ({}s remaining)",
                self.remaining().as_secs_f64() as u32
            )));
        }
        Ok(())
    }

    /// Policy floors before any outbound request to this URL's host.
    fn pace(&mut self, url: &Url) -> Result<(), Fail> {
        let Some(host) = url.host_str() else {
            return Err(Fail::Fatal("URL has no host".into()));
        };
        let class = host_class(host);
        let crawl_delay = self.crawl_delays.get(host);
        let state = pacing::load_state(&self.state_dir);
        let wait_ms = pacing::required_wait_ms(&state, pacing::now_ms(), class, crawl_delay);
        if wait_ms == 0 {
            return Ok(());
        }
        let wait = Duration::from_millis(wait_ms);
        self.budget_check(wait, "pacing wait")?;
        if wait_ms >= 100 {
            let why = match (
                state.last_request_duration_ms.unwrap_or(0) > pacing::EXPENSIVE_THRESHOLD_MS,
                class,
            ) {
                (true, _) => "last request took >1s; pausing per Robot policy",
                (false, HostClass::OtherServices) => {
                    "≥1s between requests to this service per Robot policy"
                }
                (false, HostClass::Default) => "pacing per Robot policy",
            };
            eprintln!(
                "wm-fetch: pacing: waiting {:.1}s ({why})",
                wait.as_secs_f64()
            );
        }
        std::thread::sleep(wait);
        Ok(())
    }

    /// One paced, state-recorded HTTP request (no retries here).
    fn send_once(&mut self, url: &Url) -> Result<reqwest::blocking::Response, Fail> {
        self.pace(url)?;
        let remaining = self.remaining();
        if remaining.is_zero() {
            return Err(Fail::Budget("--max-time exhausted before request".into()));
        }
        let t0 = Instant::now();
        let resp = self
            .client
            .get(url.clone())
            .header(reqwest::header::USER_AGENT, &self.ua)
            .timeout(remaining)
            .send();
        pacing::record_request(&self.state_dir, t0);
        match resp {
            Ok(r) => Ok(r),
            Err(e) => {
                if e.is_timeout() {
                    Err(Fail::Fatal("request timed out (--max-time)".into()))
                } else {
                    Err(Fail::Fatal(format!("request failed: {e}")))
                }
            }
        }
    }

    fn retry_after_secs(headers: &reqwest::header::HeaderMap, attempt: usize) -> f64 {
        let raw = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim);
        if let Some(raw) = raw {
            if let Ok(n) = raw.parse::<f64>() {
                return n.max(0.0);
            }
            if let Ok(t) = httpdate::parse_http_date(raw) {
                let now = std::time::SystemTime::now();
                if let Ok(d) = t.duration_since(now) {
                    return d.as_secs_f64();
                }
                return 0.0;
            }
        }
        // Exponential fallback: 2, 4, 8, 16s …
        2f64.powi(attempt as i32)
    }

    fn body_of(
        mut resp: reqwest::blocking::Response,
    ) -> Result<(reqwest::header::HeaderMap, Vec<u8>), Fail> {
        let headers = resp.headers().clone();
        let mut body = Vec::new();
        resp.read_to_end(&mut body)
            .map_err(|e| Fail::Fatal(format!("reading response body: {e}")))?;
        Ok((headers, body))
    }

    /// Cooldown gate + scoped robots enforcement for a URL. API-exempt
    /// surfaces (api.php, /api/, /rest.php) skip the robots check — see
    /// README "robots.txt".
    fn policy_gate(&mut self, url: &Url) -> Result<(), Fail> {
        let Some(host) = url.host_str().map(str::to_string) else {
            return Err(Fail::Fatal("URL has no host".into()));
        };
        if host_class(&host) == HostClass::OtherServices {
            if let Some(until) = pacing::cooldown_until(&self.state_dir, &host) {
                return Err(Fail::Policy(format!(
                    "Robot policy requires a ≥15-minute pause after a server error on {host}; retry after {}",
                    fmt_hhmm(until)
                )));
            }
        }
        if is_api_exempt(url) {
            return Ok(());
        }
        match self.fetch_robots(&host, url)? {
            RobotsFetch::NotFound => Ok(()), // RFC 9309: 4xx → allow-all
            RobotsFetch::Body(body) => {
                let product = self.cfg.client_name.clone();
                if let Some(delay) = robots::crawl_delay_ms(&body, &product) {
                    self.crawl_delays.set(&host, delay);
                }
                match robots::evaluate(&body, &product, url) {
                    Verdict::Allowed => Ok(()),
                    Verdict::Disallowed => {
                        let mut path = url.path().to_string();
                        if let Some(q) = url.query() {
                            path.push('?');
                            path.push_str(q);
                        }
                        Err(Fail::Policy(format!(
                            "robots.txt on {host} disallows {path} (https://{host}/robots.txt). \
                             Robot policy requires honoring it: {}. \
                             For machine-readable data use the Action API (…/w/api.php) or REST API instead; \
                             for bulk content use dumps (https://dumps.wikimedia.org).",
                            robots::ROBOT_POLICY_URL
                        )))
                    }
                }
            }
            RobotsFetch::Unreachable => Err(Fail::Policy(format!(
                "robots.txt unreachable on {host} — refusing per RFC 9309; retry shortly"
            ))),
        }
    }

    /// Fetch and cache robots.txt with the same backoff machinery as the
    /// target. Failure semantics (fail-closed per RFC 9309): 4xx →
    /// allow-all; network error / 5xx / >5 redirect hops → cached copy (any
    /// age) if present, else Unreachable.
    fn fetch_robots(&mut self, host: &str, target: &Url) -> Result<RobotsFetch, Fail> {
        if let Some(body) = robots::cache_fresh(&self.state_dir, host) {
            return Ok(RobotsFetch::Body(body));
        }
        let robots_url = robots::robots_url_for(target);
        // `--retries N` means N retries: N+1 total attempts (>= 1).
        let attempts = self.cfg.max_retries + 1;
        let mut attempt = 1;
        let mut hops = 0;
        let mut current = robots_url;
        loop {
            let resp = match self.send_once(&current) {
                Ok(r) => r,
                // Transport errors while fetching robots.txt → fail-closed
                // path. Budget aborts (Fail::Budget) propagate — they are a
                // local policy decision, not a robots-fetch failure.
                Err(Fail::Fatal(_)) => return Ok(self.robots_failure(host)),
                Err(other) => return Err(other),
            };
            let status = resp.status().as_u16();
            if (300..400).contains(&status) {
                if hops >= 5 {
                    // RFC 9309: follow at least five redirects; beyond → failure.
                    return Ok(self.robots_failure(host));
                }
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                match loc.and_then(|l| current.join(&l).ok()) {
                    Some(next) => {
                        hops += 1;
                        current = next;
                        continue;
                    }
                    None => return Ok(self.robots_failure(host)),
                }
            }
            if status == 429 || status == 503 {
                // Same backoff as the target URL. An other-services 5xx is
                // not retried: cooldown recorded, failure semantics govern.
                if cooldown_triggered(status, host_class(host)) {
                    let until = pacing::record_cooldown(&self.state_dir, host);
                    eprintln!(
                        "wm-fetch: HTTP 503 fetching robots.txt from {host}; Robot policy requires a ≥15-minute pause; retry after {}",
                        fmt_hhmm(until)
                    );
                    return Ok(self.robots_failure(host));
                }
                if attempt >= attempts {
                    return Ok(self.robots_failure(host));
                }
                let (hdrs, _) = Self::body_of(resp)?;
                let wait = Self::retry_after_secs(&hdrs, attempt);
                self.budget_check(Duration::from_secs_f64(wait), "robots.txt retry backoff")?;
                eprintln!(
                    "wm-fetch: HTTP {status} fetching robots.txt, backing off {wait:.0}s (attempt {attempt}/{attempts})"
                );
                std::thread::sleep(Duration::from_secs_f64(wait));
                attempt += 1;
                continue;
            }
            if (200..300).contains(&status) {
                let (_, body) = Self::body_of(resp)?;
                let text = String::from_utf8_lossy(&body).into_owned();
                robots::write_cache(&self.state_dir, host, &text);
                return Ok(RobotsFetch::Body(text));
            }
            if (400..500).contains(&status) {
                return Ok(RobotsFetch::NotFound);
            }
            // Remaining failures (any 5xx, odd statuses). A 5xx from an
            // other-services host arms the cooldown on the way out.
            if cooldown_triggered(status, host_class(host)) {
                let until = pacing::record_cooldown(&self.state_dir, host);
                eprintln!(
                    "wm-fetch: HTTP {status} fetching robots.txt from {host}; Robot policy requires a ≥15-minute pause; retry after {}",
                    fmt_hhmm(until)
                );
            }
            return Ok(self.robots_failure(host));
        }
    }

    fn robots_failure(&self, host: &str) -> RobotsFetch {
        match robots::cache_any_age(&self.state_dir, host) {
            Some(body) => {
                eprintln!("wm-fetch: robots.txt fetch failed for {host}; using stale cached copy");
                RobotsFetch::Body(body)
            }
            None => RobotsFetch::Unreachable,
        }
    }

    /// The main fetch loop: policy gate → paced request → retry/backoff →
    /// manual redirects → maxlag sniff. Returns the final response for
    /// output handling by the caller.
    pub fn fetch(&mut self, url: &Url) -> Result<Final, Fail> {
        // `--retries N` means N retries: N+1 total attempts (>= 1).
        let attempts = self.cfg.max_retries + 1;
        let mut attempt = 1;
        let mut hops = 0;
        let mut current = url.clone();
        loop {
            self.policy_gate(&current)?;
            let resp = self.send_once(&current)?;
            let status = resp.status().as_u16();

            // Manual redirect following: every hop is re-classified,
            // robots-checked, paced, and state-recorded.
            if (300..400).contains(&status) {
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                match loc.and_then(|l| current.join(&l).ok()) {
                    Some(next) => {
                        hops += 1;
                        if hops > self.cfg.max_redirs {
                            return Err(Fail::Fatal(format!(
                                "too many redirects (limit {})",
                                self.cfg.max_redirs
                            )));
                        }
                        // Action API redirect targets get maxlag too — a
                        // hop landing on api.php must not lose injection.
                        let injected = maxlag::inject(next.as_str(), self.cfg.maxlag);
                        current = Url::parse(&injected).map_err(|e| {
                            Fail::Fatal(format!("redirect target unparseable: {e}"))
                        })?;
                        continue;
                    }
                    None => {
                        // A 3xx without a parseable Location is not
                        // actionable — surface it as a failure.
                        let (hdrs, body) = Self::body_of(resp)?;
                        let _ = hdrs;
                        return Ok(Final {
                            status,
                            body,
                            failure: true,
                        });
                    }
                }
            }

            // 429/503 with Retry-After backoff.
            if status == 429 || status == 503 {
                let host = current.host_str().unwrap_or_default().to_string();
                if cooldown_triggered(status, host_class(&host)) {
                    // Other-services 5xx: never retried; cooldown recorded.
                    let (_, body) = Self::body_of(resp)?;
                    let until = pacing::record_cooldown(&self.state_dir, &host);
                    eprintln!(
                        "wm-fetch: HTTP 503 from {host}; Robot policy requires a ≥15-minute pause; retry after {}",
                        fmt_hhmm(until)
                    );
                    return Ok(Final {
                        status,
                        body,
                        failure: true,
                    });
                }
                let (hdrs, body) = Self::body_of(resp)?;
                if attempt >= attempts {
                    eprintln!("wm-fetch: giving up after {attempt} attempts (HTTP {status})");
                    return Ok(Final {
                        status,
                        body,
                        failure: true,
                    });
                }
                let wait = Self::retry_after_secs(&hdrs, attempt);
                self.budget_check(Duration::from_secs_f64(wait), "retry backoff")?;
                eprintln!(
                    "wm-fetch: HTTP {status}, backing off {wait:.0}s (attempt {attempt}/{attempts})"
                );
                std::thread::sleep(Duration::from_secs_f64(wait));
                attempt += 1;
                continue;
            }

            // Any other 5xx from an other-services host: not retried (only
            // 429/503 retry above), and the 15-minute cooldown is armed.
            if cooldown_triggered(status, host_class(current.host_str().unwrap_or_default())) {
                let host = current.host_str().unwrap_or_default().to_string();
                let (_, body) = Self::body_of(resp)?;
                let until = pacing::record_cooldown(&self.state_dir, &host);
                eprintln!(
                    "wm-fetch: HTTP {status} from {host}; Robot policy requires a ≥15-minute pause; retry after {}",
                    fmt_hhmm(until)
                );
                return Ok(Final {
                    status,
                    body,
                    failure: true,
                });
            }

            // Action API maxlag: the lag error arrives as HTTP 200 JSON.
            if status == 200 && surface(&current) == Surface::ActionApi {
                let (hdrs, body) = Self::body_of(resp)?;
                if let Some(ml) = maxlag::sniff_error(&body) {
                    if let Some(lag) = hdrs.get("x-database-lag").and_then(|v| v.to_str().ok()) {
                        eprintln!("wm-fetch: X-Database-Lag: {lag}");
                    }
                    if attempt >= attempts {
                        eprintln!(
                            "wm-fetch: giving up after {attempt} attempts (maxlag error persists)"
                        );
                        return Ok(Final {
                            status,
                            body,
                            failure: true,
                        });
                    }
                    let wait = Self::retry_after_secs(&hdrs, attempt).max(5.0);
                    self.budget_check(Duration::from_secs_f64(wait), "maxlag backoff")?;
                    eprintln!(
                        "wm-fetch: maxlag error ({}), waiting {wait:.0}s (attempt {attempt}/{attempts})",
                        ml.info.as_deref().unwrap_or("replica lag")
                    );
                    std::thread::sleep(Duration::from_secs_f64(wait));
                    attempt += 1;
                    continue;
                }
                return Ok(Final {
                    status,
                    body,
                    failure: false,
                });
            }

            let (_, body) = Self::body_of(resp)?;
            return Ok(Final {
                status,
                body,
                failure: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn retry_after_parsing() {
        use super::Session;
        let mut h = reqwest::header::HeaderMap::new();
        // Missing header → exponential fallback 2^attempt.
        assert_eq!(Session::retry_after_secs(&h, 1), 2.0);
        assert_eq!(Session::retry_after_secs(&h, 3), 8.0);

        // Numeric seconds.
        h.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(Session::retry_after_secs(&h, 1), 3.0);
        h.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(Session::retry_after_secs(&h, 2), 0.0);

        // HTTP-date form: 60s in the future.
        let fut = httpdate::fmt_http_date(
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
        );
        h.insert(reqwest::header::RETRY_AFTER, fut.parse().unwrap());
        let got = Session::retry_after_secs(&h, 3);
        assert!((57.0..=60.0).contains(&got), "{got}");

        // Garbage → exponential.
        h.insert(reqwest::header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(Session::retry_after_secs(&h, 3), 8.0);
    }
}
