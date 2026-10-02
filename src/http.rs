//! HTTP session: paced, locked, robots-checked, manually-redirected
//! fetching. Every outbound request — target, robots.txt, redirect hops —
//! goes through the same pacing/lock/state machinery. No request kind is
//! exempt.
//!
//! The library entry point is [`Session::connect`], which refuses to build
//! without configured operator contact (the fail-closed UA gate) and takes
//! the state directory as an explicit parameter. [`SessionOptions`] lets a
//! library consumer opt into raw wire capture, internal-address refusal, a
//! per-response body cap, and record-only robots mode — the CLI uses the
//! defaults, which preserve its historical behaviour.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use url::Url;

use crate::classify::{cooldown_triggered, host_class, is_api_exempt, surface, HostClass, Surface};
use crate::config::Config;
use crate::maxlag;
use crate::pacing::{self, fmt_hhmm};
use crate::robots::{self, Report, ReportVerdict, Verdict};
use crate::ssrf;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_POLICY: i32 = 3;

/// Errors that end the invocation.
#[derive(Debug)]
pub enum Fail {
    /// Exit 2: configuration problem — in practice, no operator contact
    /// configured (the fail-closed UA gate refused to build a session).
    Config(String),
    /// Exit 1: transport failure, HTTP error after retries.
    Fatal(String),
    /// Exit 1: the wall-clock --max-time budget could not cover a required
    /// wait (pacing, lock, backoff). Distinct from Fatal so robots-fetch
    /// error handling does not swallow it into "robots.txt unreachable".
    Budget(String),
    /// Exit 3: policy refusal — robots.txt disallow (enforce mode),
    /// robots.txt unreachable after cache fallback (enforce mode),
    /// other-services 5xx cooldown, or an internal-address target.
    Policy(String),
    /// Exit 1: the response body exceeded the configured per-response cap.
    /// No partial body is returned.
    TooLarge { limit: u64, observed: u64 },
}

/// How robots.txt verdicts are handled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RobotsMode {
    /// Disallowed targets are refused (exit 3); unreachable robots.txt is
    /// fail-closed. The CLI default and the library default.
    #[default]
    Enforce,
    /// The robots.txt verdict is consulted, paced (crawl-delay still
    /// applies), recorded on the result — and the fetch proceeds anyway.
    /// Lets a downstream tool own its robots posture with recording built
    /// in (citation-fetcher uses this for non-Wikimedia cited sources).
    RecordOnly,
}

/// Library-consumer options. Defaults preserve CLI behaviour exactly.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    /// Robots handling. Default [`RobotsMode::Enforce`].
    pub robots_mode: RobotsMode,
    /// Return the response body as received on the wire (gzip
    /// auto-decoding disabled) with per-hop header records, so WARC
    /// records match their `Content-Encoding`. Use [`decode_body`] to
    /// decode. Default false.
    pub raw_capture: bool,
    /// Refuse IP-literal targets and redirect hops that resolve to
    /// non-public addresses (SSRF guard, ported from SP42). Default false
    /// (the CLI talks to Wikimedia hosts; the guard is opt-in).
    pub refuse_internal_addresses: bool,
    /// Per-HTTP-response body cap in bytes. A response that exceeds it
    /// yields [`Fail::TooLarge`] with no partial body. Default: no cap.
    pub max_body_bytes: Option<u64>,
}

/// One outbound hop of a fetch: the request headers we set, the status, and
/// the response headers as received.
#[derive(Debug, Clone)]
pub struct Hop {
    pub url: Url,
    /// Headers explicitly set on the request (User-Agent; reqwest's
    /// automatic Accept-Encoding is not observable here).
    pub request_headers: Vec<(String, String)>,
    pub status: u16,
    pub response_headers: reqwest::header::HeaderMap,
}

/// The robots verdict consulted for the final hop, when robots.txt was
/// consulted at all (API-exempt surfaces are None).
pub type RobotsReport = Report;

#[derive(Debug)]
pub struct Final {
    pub status: u16,
    /// Decoded body, or — in raw-capture mode — the body as received on
    /// the wire (see [`SessionOptions::raw_capture`] and [`decode_body`]).
    pub body: Vec<u8>,
    /// True when the response is being returned only because retries were
    /// exhausted (e.g. a persistent HTTP-200 maxlag error): body still goes
    /// to stdout, but the invocation is a failure (exit 1).
    pub failure: bool,
    /// Every outbound hop of this fetch (redirect chain, final response).
    pub hops: Vec<Hop>,
    /// The robots.txt verdict for the final hop, when consulted.
    pub robots: Option<RobotsReport>,
}

/// Decode a wire-captured body according to its `Content-Encoding`.
/// `identity` (or absent) passes through; `gzip` is decompressed.
///
/// # Errors
/// Returns a message for unsupported encodings or corrupt gzip data.
pub fn decode_body(headers: &reqwest::header::HeaderMap, body: &[u8]) -> Result<Vec<u8>, String> {
    let enc = headers
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity")
        .trim()
        .to_ascii_lowercase();
    match enc.as_str() {
        "" | "identity" => Ok(body.to_vec()),
        "gzip" | "x-gzip" => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(body)
                .read_to_end(&mut out)
                .map_err(|e| format!("gunzip: {e}"))?;
            Ok(out)
        }
        other => Err(format!("unsupported content-encoding {other:?}")),
    }
}

/// Format an error with its full source chain, so resolver-level refusals
/// (e.g. the SSRF guard's "host resolved only to non-public addresses")
/// stay visible instead of hiding behind reqwest's top-level message.
fn format_error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        msg.push_str("; caused by: ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}

pub struct Session {
    cfg: Config,
    ua: String,
    state_dir: PathBuf,
    deadline: Instant,
    client: reqwest::blocking::Client,
    crawl_delays: robots::CrawlDelays,
    options: SessionOptions,
    /// Per-fetch accumulation: redirect hops of the current fetch().
    hops: Vec<Hop>,
    /// Per-fetch accumulation: robots verdict consulted for the final hop.
    robots_report: Option<RobotsReport>,
    /// The lock bucket currently held. Wikimedia and other-Wikimedia-
    /// services hosts share the global `state.lock`, held for the session
    /// lifetime (pre-2.1 machine-wide serialization, unchanged). Default
    /// hosts hold a per-host lock plus one machine-wide concurrency slot,
    /// released at the end of a fetch.
    lock_state: BucketLock,
}

enum BucketLock {
    None,
    /// Machine-wide serialization for Wikimedia hosts — exactly the
    /// pre-2.1 behaviour. Held until the session drops.
    Global {
        _lock: fs::File,
    },
    /// Per-host serialization plus a slot in the machine-wide concurrency
    /// pool (cap `Config::global_concurrency`). Released at fetch end.
    Host {
        host: String,
        _host_lock: fs::File,
        _slot: fs::File,
    },
}

enum RobotsFetch {
    Body(String),
    /// HTTP 4xx — RFC 9309: allow-all.
    NotFound,
    /// Network error / 5xx / too many redirects — fail-closed path.
    Unreachable,
}

impl Session {
    /// Build a session with default options. The constructor refuses to
    /// build without configured contact, so library consumers inherit the
    /// UA policy automatically.
    ///
    /// # Errors
    /// [`Fail::Config`] when no contact is configured (and no custom UA);
    /// see [`Session::connect_with`].
    pub fn connect(cfg: Config, state_dir: PathBuf) -> Result<Self, Fail> {
        Self::connect_with(cfg, state_dir, SessionOptions::default())
    }

    /// Build a session with explicit options. The fail-closed contact gate
    /// runs here: `cfg` must carry a `user_agent`, `contact_email`, or
    /// `contact_page`. The UA is constructed inside, never passed in, so
    /// no consumer can bypass the policy by accident.
    ///
    /// # Errors
    /// [`Fail::Config`] without contact; [`Fail::Fatal`] when the HTTP
    /// client or the lock cannot be built; [`Fail::Budget`] when the lock
    /// wait exceeds `--max-time`.
    pub fn connect_with(
        cfg: Config,
        state_dir: PathBuf,
        options: SessionOptions,
    ) -> Result<Self, Fail> {
        // Fail-closed contact gate: never send an anonymous request. This
        // is the fix for the "fork ships the upstream author's contact"
        // bug — a fresh fork refuses to run until the operator configures
        // their own.
        if cfg.user_agent.is_none() && cfg.contact_email.is_none() && cfg.contact_page.is_none() {
            return Err(Fail::Config(
                "no contact configured: the WMF User-Agent policy requires contact info in \
                 every User-Agent. Set contact_email/contact_page (config file, \
                 WM_FETCH_CONTACT_* env, or --contact-*), or pass --user-agent with your \
                 own registered agent. Run `wm-fetch --init` to scaffold a config."
                    .to_string(),
            ));
        }

        if let Some(custom) = &cfg.user_agent {
            if !crate::ua::has_contact_group(custom) {
                eprintln!(
                    "wm-fetch: custom --user-agent carries no parenthesized contact group; the UA \
                     policy expects one. Sending it anyway — the obligation is yours."
                );
            }
        }

        let ua = cfg.user_agent.clone().unwrap_or_else(|| {
            crate::ua::build(
                &cfg.client_name,
                cfg.contact_page.as_deref(),
                cfg.contact_email.as_deref(),
            )
        });
        Self::build(cfg, ua, state_dir, options)
    }

    fn build(
        cfg: Config,
        ua: String,
        state_dir: PathBuf,
        options: SessionOptions,
    ) -> Result<Self, Fail> {
        let connect_timeout = Duration::from_secs_f64(cfg.connect_timeout_secs.max(0.1));
        let mut builder = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none()) // hops are driven manually
            .connect_timeout(connect_timeout);
        if options.raw_capture {
            // Wire bytes: no automatic gzip negotiation/decoding, so the
            // stored body matches its Content-Encoding.
            builder = builder.gzip(false);
        }
        if options.refuse_internal_addresses {
            // The resolver guard runs for every connection (including each
            // manually driven redirect hop); no_proxy() keeps a deployment
            // proxy from moving resolution out from under it.
            builder = builder
                .dns_resolver(std::sync::Arc::new(ssrf::GuardedResolver::system()))
                .no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| Fail::Fatal(format!("building HTTP client: {e}")))?;

        let deadline = Instant::now() + Duration::from_secs_f64(cfg.max_time_secs.max(0.1));

        // The lock is acquired lazily, per bucket, at fetch time: Wikimedia
        // hosts serialize machine-wide via the global state.lock (held for
        // the session); Default hosts take a per-host lock plus a
        // concurrency-pool slot.
        fs::create_dir_all(&state_dir)
            .map_err(|e| Fail::Fatal(format!("creating state dir {}: {e}", state_dir.display())))?;

        Ok(Self {
            cfg,
            ua,
            state_dir,
            deadline,
            client,
            crawl_delays: robots::CrawlDelays::default(),
            options,
            hops: Vec::new(),
            robots_report: None,
            lock_state: BucketLock::None,
        })
    }

    /// Wait for an exclusive lock on `path` until the deadline.
    fn acquire_exclusive(path: &PathBuf, deadline: Instant, what: &str) -> Result<fs::File, Fail> {
        use fs4::fs_std::FileExt;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Fail::Fatal(format!("creating {}: {e}", parent.display())))?;
        }
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| Fail::Fatal(format!("opening {}: {e}", path.display())))?;
        loop {
            if matches!(lock.try_lock_exclusive(), Ok(true)) {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(Fail::Budget(format!(
                    "{what} wait would exceed --max-time (another wm-fetch is running)"
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Take one slot from the machine-wide concurrency pool (Default hosts
    /// only). Slots are scanned in a fixed order so there is no
    /// lock-ordering cycle; waits are bounded by the budget.
    fn acquire_slot(&self) -> Result<fs::File, Fail> {
        use fs4::fs_std::FileExt;
        let cap = self.cfg.global_concurrency.max(1);
        loop {
            for i in 0..cap {
                let path = pacing::slot_file(&self.state_dir, i);
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Ok(f) = fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(&path)
                {
                    if matches!(f.try_lock_exclusive(), Ok(true)) {
                        return Ok(f);
                    }
                }
            }
            if Instant::now() >= self.deadline {
                return Err(Fail::Budget(
                    "concurrency-slot wait would exceed --max-time (all global slots busy)"
                        .to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Acquire the right lock bucket for a request to `url`'s host.
    fn ensure_lock(&mut self, url: &Url) -> Result<(), Fail> {
        let Some(host) = url.host_str().map(str::to_string) else {
            return Err(Fail::Fatal("URL has no host".into()));
        };
        match host_class(&host) {
            HostClass::Wikimedia | HostClass::OtherServices => {
                if !matches!(self.lock_state, BucketLock::Global { .. }) {
                    self.release_host_lock();
                    let lock = Self::acquire_exclusive(
                        &self.state_dir.join("state.lock"),
                        self.deadline,
                        "lock",
                    )?;
                    self.lock_state = BucketLock::Global { _lock: lock };
                }
            }
            HostClass::Default => {
                if let BucketLock::Host { host: held, .. } = &self.lock_state {
                    if *held == host {
                        return Ok(());
                    }
                }
                self.release_host_lock();
                let slot = self.acquire_slot()?;
                let host_lock = Self::acquire_exclusive(
                    &pacing::host_lock_file(&self.state_dir, &host),
                    self.deadline,
                    "per-host lock",
                )?;
                self.lock_state = BucketLock::Host {
                    host,
                    _host_lock: host_lock,
                    _slot: slot,
                };
            }
        }
        Ok(())
    }

    /// Drop a held per-host lock (the slot goes with it). The global
    /// Wikimedia lock is never released early.
    fn release_host_lock(&mut self) {
        if matches!(self.lock_state, BucketLock::Host { .. }) {
            self.lock_state = BucketLock::None;
        }
    }

    /// The User-Agent this session sends.
    #[must_use]
    pub fn user_agent(&self) -> &str {
        &self.ua
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
        let state_file = pacing::state_file_for(&self.state_dir, host);
        let state = pacing::load_state_file(&state_file);
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
                (false, HostClass::Default) => "≥1s per-host pacing floor",
                (false, HostClass::Wikimedia) => "pacing per Robot policy",
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
        let state_file =
            pacing::state_file_for(&self.state_dir, url.host_str().unwrap_or_default());
        pacing::record_request_file(&state_file, t0);
        match resp {
            Ok(r) => Ok(r),
            Err(e) => {
                if e.is_timeout() {
                    Err(Fail::Fatal("request timed out (--max-time)".into()))
                } else {
                    Err(Fail::Fatal(format!(
                        "request failed: {}",
                        format_error_chain(&e)
                    )))
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
        cap: Option<u64>,
    ) -> Result<(reqwest::header::HeaderMap, Vec<u8>), Fail> {
        let headers = resp.headers().clone();
        let mut body = Vec::new();
        match cap {
            None => {
                resp.read_to_end(&mut body)
                    .map_err(|e| Fail::Fatal(format!("reading response body: {e}")))?;
            }
            Some(limit) => {
                // Read one byte past the cap to detect exceed without
                // draining an unbounded body.
                (&mut resp)
                    .take(limit.saturating_add(1))
                    .read_to_end(&mut body)
                    .map_err(|e| Fail::Fatal(format!("reading response body: {e}")))?;
                if body.len() as u64 > limit {
                    return Err(Fail::TooLarge {
                        limit,
                        observed: body.len() as u64,
                    });
                }
            }
        }
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
            RobotsFetch::NotFound => {
                // RFC 9309: 4xx → allow-all, no rules matched.
                self.robots_report = Some(Report {
                    verdict: ReportVerdict::Allowed,
                    matched_rules: Vec::new(),
                });
                Ok(())
            }
            RobotsFetch::Body(body) => {
                let product = self.cfg.client_name.clone();
                if let Some(delay) = robots::crawl_delay_ms(&body, &product) {
                    self.crawl_delays.set(&host, delay);
                }
                let rules = robots::matched_rules(&body, &product, url);
                match robots::evaluate(&body, &product, url) {
                    Verdict::Allowed => {
                        self.robots_report = Some(Report {
                            verdict: ReportVerdict::Allowed,
                            matched_rules: rules,
                        });
                        Ok(())
                    }
                    Verdict::Disallowed => {
                        self.robots_report = Some(Report {
                            verdict: ReportVerdict::Disallowed,
                            matched_rules: rules,
                        });
                        if self.options.robots_mode == RobotsMode::RecordOnly {
                            // Operator posture: record, fetch anyway.
                            // Crawl-delay still applied above.
                            return Ok(());
                        }
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
            RobotsFetch::Unreachable => match self.options.robots_mode {
                RobotsMode::RecordOnly => {
                    self.robots_report = Some(Report {
                        verdict: ReportVerdict::NoRobots,
                        matched_rules: Vec::new(),
                    });
                    Ok(())
                }
                RobotsMode::Enforce => Err(Fail::Policy(format!(
                    "robots.txt unreachable on {host} — refusing per RFC 9309; retry shortly"
                ))),
            },
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
                let (hdrs, _) = Self::body_of(resp, self.options.max_body_bytes)?;
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
                let (_, body) = Self::body_of(resp, self.options.max_body_bytes)?;
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

    fn record_hop(&mut self, url: &Url, status: u16, headers: &reqwest::header::HeaderMap) {
        self.hops.push(Hop {
            url: url.clone(),
            request_headers: vec![("user-agent".to_string(), self.ua.clone())],
            status,
            response_headers: headers.clone(),
        });
    }

    /// The main fetch loop: policy gate → paced request → retry/backoff →
    /// manual redirects → maxlag sniff. Returns the final response for
    /// output handling by the caller.
    pub fn fetch(&mut self, url: &Url) -> Result<Final, Fail> {
        let result = self.fetch_inner(url);
        // A per-host lock (and its concurrency slot) is released at the end
        // of a fetch; the global Wikimedia lock is held for the session.
        self.release_host_lock();
        result
    }

    fn fetch_inner(&mut self, url: &Url) -> Result<Final, Fail> {
        self.hops.clear();
        self.robots_report = None;
        // `--retries N` means N retries: N+1 total attempts (>= 1).
        let attempts = self.cfg.max_retries + 1;
        let mut attempt = 1;
        let mut hops = 0;
        let mut current = url.clone();
        loop {
            if self.options.refuse_internal_addresses && ssrf::host_is_blocked_literal(&current) {
                return Err(Fail::Policy(format!(
                    "SSRF: target {} is a non-public IP literal",
                    current
                )));
            }
            self.ensure_lock(&current)?;
            self.policy_gate(&current)?;
            let resp = self.send_once(&current)?;
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            self.record_hop(&current, status, &resp_headers);

            // Manual redirect following: every hop is re-classified,
            // robots-checked, paced, and state-recorded.
            if (300..400).contains(&status) {
                let loc = resp_headers
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
                        let (_, body) = Self::body_of(resp, self.options.max_body_bytes)?;
                        return Ok(self.finalize(Final {
                            status,
                            body,
                            failure: true,
                            hops: Vec::new(),
                            robots: None,
                        }));
                    }
                }
            }

            // 429/503 with Retry-After backoff.
            if status == 429 || status == 503 {
                let host = current.host_str().unwrap_or_default().to_string();
                if cooldown_triggered(status, host_class(&host)) {
                    // Other-services 5xx: never retried; cooldown recorded.
                    let (_, body) = Self::body_of(resp, self.options.max_body_bytes)?;
                    let until = pacing::record_cooldown(&self.state_dir, &host);
                    eprintln!(
                        "wm-fetch: HTTP 503 from {host}; Robot policy requires a ≥15-minute pause; retry after {}",
                        fmt_hhmm(until)
                    );
                    return Ok(self.finalize(Final {
                        status,
                        body,
                        failure: true,
                        hops: Vec::new(),
                        robots: None,
                    }));
                }
                let (hdrs, body) = Self::body_of(resp, self.options.max_body_bytes)?;
                if attempt >= attempts {
                    eprintln!("wm-fetch: giving up after {attempt} attempts (HTTP {status})");
                    return Ok(self.finalize(Final {
                        status,
                        body,
                        failure: true,
                        hops: Vec::new(),
                        robots: None,
                    }));
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
                let (_, body) = Self::body_of(resp, self.options.max_body_bytes)?;
                let until = pacing::record_cooldown(&self.state_dir, &host);
                eprintln!(
                    "wm-fetch: HTTP {status} from {host}; Robot policy requires a ≥15-minute pause; retry after {}",
                    fmt_hhmm(until)
                );
                return Ok(self.finalize(Final {
                    status,
                    body,
                    failure: true,
                    hops: Vec::new(),
                    robots: None,
                }));
            }

            // Action API maxlag: the lag error arrives as HTTP 200 JSON.
            if status == 200 && surface(&current) == Surface::ActionApi {
                let (hdrs, body) = Self::body_of(resp, self.options.max_body_bytes)?;
                if let Some(ml) = maxlag::sniff_error(&body) {
                    if let Some(lag) = hdrs.get("x-database-lag").and_then(|v| v.to_str().ok()) {
                        eprintln!("wm-fetch: X-Database-Lag: {lag}");
                    }
                    if attempt >= attempts {
                        eprintln!(
                            "wm-fetch: giving up after {attempt} attempts (maxlag error persists)"
                        );
                        return Ok(self.finalize(Final {
                            status,
                            body,
                            failure: true,
                            hops: Vec::new(),
                            robots: None,
                        }));
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
                return Ok(self.finalize(Final {
                    status,
                    body,
                    failure: false,
                    hops: Vec::new(),
                    robots: None,
                }));
            }

            let (_, body) = Self::body_of(resp, self.options.max_body_bytes)?;
            return Ok(self.finalize(Final {
                status,
                body,
                failure: false,
                hops: Vec::new(),
                robots: None,
            }));
        }
    }

    /// Attach the accumulated hops and robots report to a Final.
    fn finalize(&mut self, mut final_resp: Final) -> Final {
        final_resp.hops = std::mem::take(&mut self.hops);
        final_resp.robots = self.robots_report.clone();
        final_resp
    }
}

#[cfg(test)]
mod tests {
    use super::Session;

    #[test]
    fn retry_after_parsing() {
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

    #[test]
    fn decode_body_identity_and_gzip() {
        use super::decode_body;
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(decode_body(&h, b"plain").unwrap(), b"plain");
        h.insert(reqwest::header::CONTENT_ENCODING, "gzip".parse().unwrap());
        let mut gz = flate2::read::GzEncoder::new(
            std::io::Cursor::new(b"compressed hello".to_vec()),
            flate2::Compression::default(),
        );
        let mut wire = Vec::new();
        std::io::Read::read_to_end(&mut gz, &mut wire).unwrap();
        assert_eq!(
            decode_body(&h, &wire).unwrap(),
            b"compressed hello".to_vec()
        );
        h.insert(reqwest::header::CONTENT_ENCODING, "br".parse().unwrap());
        assert!(decode_body(&h, b"x").is_err());
    }
}
