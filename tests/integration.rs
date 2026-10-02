//! Integration tests: run the real binary against local mock servers.
//! Offline (wiremock + raw TcpListener) — no Wikimedia traffic here; live
//! legs live in tests/live.rs behind #[ignore].
//!
//! Each test gets a fresh state dir (pacing/lock/robots cache isolation)
//! and an isolated config (WM_FETCH_CONFIG pointing at a missing file).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_wm-fetch"))
}

fn isolated(state_dir: &Path) -> Command {
    let mut c = bin();
    c.env("WM_FETCH_STATE_DIR", state_dir)
        .env("WM_FETCH_CONFIG", state_dir.join("no-such-config.toml"))
        // Belt and braces: keep XDG defaults out of the way too.
        .env("XDG_CONFIG_HOME", state_dir.join("xdg-config"))
        .env("XDG_CACHE_HOME", state_dir.join("xdg-cache"));
    for k in [
        "WM_FETCH_CONTACT_EMAIL",
        "WM_FETCH_CONTACT_PAGE",
        "WM_FETCH_USER_AGENT",
        "WM_FETCH_CLIENT_NAME",
    ] {
        c.env_remove(k);
    }
    c
}

fn run(c: &mut Command) -> Output {
    c.output().expect("running wm-fetch binary")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

/// Allow-all robots.txt mock (mounted at /robots.txt).
async fn allow_robots(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow:\n"))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------- AC.1 ---

#[tokio::test]
async fn ua_header_sent() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/Article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello article"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/Article", server.uri()))
        .arg("--contact-email")
        .arg("tester@example.org")
        .arg("--contact-page")
        .arg("https://en.wikipedia.org/wiki/User:Tester"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "hello article");

    let reqs = server.received_requests().await.unwrap();
    let target = reqs
        .iter()
        .find(|r| r.url.path() == "/wiki/Article")
        .unwrap();
    let ua = target
        .headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .expect("user-agent header present");
    assert!(
        ua.starts_with(&format!(
            "wm-fetch-bot/{} (https://en.wikipedia.org/wiki/User:Tester tester@example.org) reqwest/",
            env!("CARGO_PKG_VERSION")
        )),
        "UA was {ua:?}"
    );
    let ae = target
        .headers
        .get("accept-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(ae.contains("gzip"), "Accept-Encoding was {ae:?}");
}

#[tokio::test]
async fn no_contact_no_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/Article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("should never be served"))
        .expect(0)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path()).arg(format!("{}/wiki/Article", server.uri())));
    assert_eq!(code(&out), 2, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("no contact configured"),
        "stderr: {}",
        stderr(&out)
    );
    server.verify().await; // zero expectations must hold
}

#[tokio::test]
async fn empty_contact_is_no_contact() {
    // Regression: an empty/whitespace --contact-email (e.g. an unset shell
    // variable) must be treated as no contact — never an anonymous UA.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/Article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("nope"))
        .expect(0)
        .mount(&server)
        .await;

    for empty in ["", "   "] {
        let dir = tempfile::tempdir().unwrap();
        let out = run(isolated(dir.path())
            .arg(format!("{}/wiki/Article", server.uri()))
            .arg("--contact-email")
            .arg(empty));
        assert_eq!(code(&out), 2, "empty={empty:?} stderr: {}", stderr(&out));
    }
    server.verify().await;
}

#[tokio::test]
async fn user_agent_override_no_contact() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/Article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let ua = "mybot/1.0 (me@example.com)";
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/Article", server.uri()))
        .arg("--user-agent")
        .arg(ua));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));

    let reqs = server.received_requests().await.unwrap();
    let target = reqs
        .iter()
        .find(|r| r.url.path() == "/wiki/Article")
        .unwrap();
    assert_eq!(
        target
            .headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok()),
        Some(ua)
    );
    // No warning for a compliant custom UA.
    assert!(
        !stderr(&out).contains("contact group"),
        "stderr: {}",
        stderr(&out)
    );
}

#[tokio::test]
async fn user_agent_override_warns() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/Article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/Article", server.uri()))
        .arg("--user-agent")
        .arg("mybot/1.0"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("no parenthesized contact group"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(stdout(&out), "ok");
}

// ---------------------------------------------------------------- AC.2 ---

#[test]
fn print_config_contactless() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path()).arg("--print-config"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("client_name"),
        "stdout: {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("REFUSES TO FETCH"),
        "stdout: {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains(&format!("wm-fetch-bot/{} (", env!("CARGO_PKG_VERSION"))),
        "stdout: {}",
        stdout(&out)
    );
}

// ---------------------------------------------------------------- AC.3 ---

#[tokio::test]
async fn maxlag_reaches_mock_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"batchcomplete":true}"#))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!(
            "{}/w/api.php?action=query&format=json",
            server.uri()
        ))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));

    let reqs = server.received_requests().await.unwrap();
    let api = reqs.iter().find(|r| r.url.path() == "/w/api.php").unwrap();
    assert!(
        api.url.query().unwrap_or("").contains("maxlag=5"),
        "query was {}",
        api.url
    );
}

// ---------------------------------------------------------------- AC.4 ---

#[tokio::test]
async fn maxlag_200_retry_then_success() {
    let server = MockServer::start().await;
    // First call: HTTP 200 with the JSON maxlag error + Retry-After.
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Retry-After", "5")
                .insert_header("X-Database-Lag", "6")
                .set_body_string(
                    r#"{"error":{"code":"maxlag","info":"Waiting for db: 6 seconds lagged"}}"#,
                ),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Then the real payload.
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"batchcomplete":true,"good":1}"#),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let t0 = Instant::now();
    let out = run(isolated(dir.path())
        .arg(format!(
            "{}/w/api.php?action=query&format=json",
            server.uri()
        ))
        .arg("--contact-email")
        .arg("t@example.org"));
    let elapsed = t0.elapsed();
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"good\":1"),
        "stdout: {}",
        stdout(&out)
    );
    // The retry floor is max(Retry-After, 5s) — a real ≥5s wait.
    assert!(
        elapsed >= Duration::from_millis(4900),
        "elapsed {:?}",
        elapsed
    );
    assert!(stderr(&out).contains("maxlag"), "stderr: {}", stderr(&out));
}

#[tokio::test]
async fn maxlag_200_exhaustion() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"error":{"code":"maxlag","info":"Waiting for db: 6 seconds lagged"}}"#,
        ))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    // --retries 0 = exactly one attempt, no sleep: still proves body→stdout + exit 1.
    let out = run(isolated(dir.path())
        .arg(format!(
            "{}/w/api.php?action=query&format=json",
            server.uri()
        ))
        .arg("--contact-email")
        .arg("t@example.org")
        .arg("--retries")
        .arg("0"));
    assert_eq!(code(&out), 1, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"maxlag\""),
        "stdout: {}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("giving up"),
        "stderr: {}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------- AC.5 ---

#[tokio::test]
async fn retry_after_numeric() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("recovered"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let t0 = Instant::now();
    let out = run(isolated(dir.path())
        .arg(format!("{}/w/api.php?x=1", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org"));
    let elapsed = t0.elapsed();
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "recovered");
    assert!(
        elapsed >= Duration::from_millis(990),
        "elapsed {:?}",
        elapsed
    );
    assert!(
        stderr(&out).contains("backing off 1s"),
        "stderr: {}",
        stderr(&out)
    );
}

#[tokio::test]
async fn retry_after_exhaustion() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("Retry-After", "1")
                .set_body_string("busy"),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/w/api.php?x=1", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org")
        .arg("--retries")
        .arg("1"));
    assert_eq!(code(&out), 1, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "busy"); // last body still printed
    assert!(
        stderr(&out).contains("giving up after 2 attempts"),
        "stderr: {}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------- AC.6 ---

#[tokio::test]
async fn http_404_body_and_exit() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/Missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"error":"missingpage"}"#))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/Missing", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 1, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), r#"{"error":"missingpage"}"#);
    assert!(
        stderr(&out).contains("HTTP 404"),
        "stderr: {}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------- AC.7 ---

#[tokio::test]
async fn redirects_bounded() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    for (from, to) in [("/a", "/b"), ("/b", "/c"), ("/c", "/d")] {
        Mock::given(method("GET"))
            .and(path(from))
            .respond_with(ResponseTemplate::new(301).insert_header("Location", to))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/d"))
        .respond_with(ResponseTemplate::new(200).set_body_string("arrived"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/a", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "arrived");

    // 4 hops with the default limit of 3 → failure. Fresh server + fresh
    // paths so the earlier /d → 200 mock cannot shadow this chain.
    let server2 = MockServer::start().await;
    allow_robots(&server2).await;
    for (from, to) in [("/p", "/q"), ("/q", "/r"), ("/r", "/s"), ("/s", "/t")] {
        Mock::given(method("GET"))
            .and(path(from))
            .respond_with(ResponseTemplate::new(301).insert_header("Location", to))
            .mount(&server2)
            .await;
    }
    let dir2 = tempfile::tempdir().unwrap();
    let out = run(isolated(dir2.path())
        .arg(format!("{}/p", server2.uri()))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 1, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("too many redirects"),
        "stderr: {}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------- AC.8 ---

#[tokio::test]
async fn total_timeout_enforced() {
    let server = MockServer::start().await;
    // API-exempt path: no robots fetch interferes with the timing.
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("late")
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let t0 = Instant::now();
    let out = run(isolated(dir.path())
        .arg(format!("{}/w/api.php?x=1", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org")
        .arg("--max-time")
        .arg("1"));
    let elapsed = t0.elapsed();
    assert_eq!(code(&out), 1, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("timed out"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(elapsed < Duration::from_secs(3), "elapsed {:?}", elapsed);
}

#[tokio::test]
async fn pacing_wait_aborts_under_max_time() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/x"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    // Pre-seed: previous request took >1s → next must wait ≥5s…
    // (2.1: non-Wikimedia hosts pace from the per-host state file.)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let hosts_dir = dir.path().join("hosts");
    std::fs::create_dir_all(&hosts_dir).unwrap();
    std::fs::write(
        hosts_dir.join("127.0.0.1.json"),
        format!(r#"{{"last_request_end_ms":{now},"last_request_duration_ms":2000}}"#),
    )
    .unwrap();

    let t0 = Instant::now();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/x", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org")
        .arg("--max-time")
        .arg("2"));
    let elapsed = t0.elapsed();
    assert_eq!(code(&out), 1, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("would exceed --max-time"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(elapsed < Duration::from_secs(2), "elapsed {:?}", elapsed);
    // …and it aborted before issuing any request.
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------- AC.9 ---

const WM_ROBOTS: &str = "User-agent: *\nAllow: /w/api.php?action=mobileview&\nDisallow: /w/\nDisallow: /api/\nDisallow: /trap/\nDisallow: /wiki/Special:\n";

async fn wm_robots(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(WM_ROBOTS))
        .mount(server)
        .await;
}

#[tokio::test]
async fn robots_refusal_no_request() {
    let server = MockServer::start().await;
    wm_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/Special:Export"))
        .respond_with(ResponseTemplate::new(200).set_body_string("secret"))
        .expect(0)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/Special:Export", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 3, "stderr: {}", stderr(&out));
    assert!(stdout(&out).is_empty(), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains("disallows /wiki/Special:Export"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("Robot policy"),
        "stderr: {}",
        stderr(&out)
    );
    server.verify().await;
}

#[tokio::test]
async fn robots_allow_article() {
    let server = MockServer::start().await;
    wm_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/Article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("article body"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/Article", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "article body");
}

#[tokio::test]
async fn robots_api_exempt() {
    let server = MockServer::start().await;
    // robots.txt disallows /w/ and /api/ outright (as Wikimedia's does) —
    // the API endpoints must still be fetched: they're governed by
    // API:Etiquette, not the crawler-oriented robots.txt.
    wm_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/w/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":1}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/rest_v1/page/summary/X"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":"rest"}"#))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    for (p, want) in [
        ("/w/api.php?action=query", r#"{"ok":1}"#),
        ("/api/rest_v1/page/summary/X", r#"{"ok":"rest"}"#),
    ] {
        let d = tempfile::tempdir().unwrap();
        let out = run(isolated(d.path())
            .arg(format!("{}{p}", server.uri()))
            .arg("--contact-email")
            .arg("t@example.org"));
        assert_eq!(code(&out), 0, "for {p}: {}", stderr(&out));
        assert_eq!(stdout(&out), want, "for {p}");
    }
    let _ = dir;
}

#[tokio::test]
async fn robots_missing_allow_all() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/Article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("fine"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/Article", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "fine");
}

#[tokio::test]
async fn robots_unreachable_refused() {
    // 5xx leg: robots.txt always 500, no retry (--retries 0), no cache → exit 3.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/Article", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org")
        .arg("--retries")
        .arg("0"));
    assert_eq!(code(&out), 3, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("robots.txt unreachable"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn robots_unreachable_network_error_refused() {
    // Network-error leg: accept-then-close — no HTTP response at all.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_thread = std::thread::spawn(move || {
        // Exactly one accept: with --retries 0 the binary issues exactly one
        // robots.txt request before giving up.
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf); // swallow the request
        drop(s); // close without responding
    });

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("http://127.0.0.1:{port}/wiki/Article"))
        .arg("--contact-email")
        .arg("t@example.org")
        .arg("--retries")
        .arg("0")
        .arg("--max-time")
        .arg("5"));
    assert_eq!(code(&out), 3, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("robots.txt unreachable"),
        "stderr: {}",
        stderr(&out)
    );
    server_thread.join().unwrap();
}

#[tokio::test]
async fn robots_redirect_to_disallowed() {
    let server = MockServer::start().await;
    wm_robots(&server).await;
    // /wiki/From is allowed; it 301s to the disallowed /wiki/Special:X.
    Mock::given(method("GET"))
        .and(path("/wiki/From"))
        .respond_with(ResponseTemplate::new(301).insert_header("Location", "/wiki/Special:X"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/Special:X"))
        .respond_with(ResponseTemplate::new(200).set_body_string("trap"))
        .expect(0)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/From", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 3, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("disallows /wiki/Special:X"),
        "stderr: {}",
        stderr(&out)
    );
    server.verify().await; // the disallowed route was never fetched
}

// --------------------------------------------------------------- AC.10 ---

#[tokio::test]
async fn pacing_after_expensive() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/x"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // (2.1: non-Wikimedia hosts pace from the per-host state file.)
    let hosts_dir = dir.path().join("hosts");
    std::fs::create_dir_all(&hosts_dir).unwrap();
    std::fs::write(
        hosts_dir.join("127.0.0.1.json"),
        format!(r#"{{"last_request_end_ms":{now},"last_request_duration_ms":1200}}"#),
    )
    .unwrap();

    let t0 = Instant::now();
    let out = run(isolated(dir.path())
        .arg(format!("{}/wiki/x", server.uri()))
        .arg("--contact-email")
        .arg("t@example.org")
        .arg("--max-time")
        .arg("30"));
    let elapsed = t0.elapsed();
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert!(
        elapsed >= Duration::from_millis(4900),
        "elapsed {:?}",
        elapsed
    );
    assert!(stderr(&out).contains("pacing"), "stderr: {}", stderr(&out));
}

/// Raw TCP server that answers instantly, recording arrival times.
/// Serves flock_serializes_concurrent_calls and robots_first_call_paced —
/// timing facts wiremock cannot observe.
struct RawServer {
    arrivals: std::sync::Arc<std::sync::Mutex<Vec<Instant>>>,
    responses: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>, // (path prefix, body)
    delay_ms: u64,
}

impl RawServer {
    fn spawn(responses: Vec<(String, String)>, delay_ms: u64) -> (std::sync::Arc<Self>, u16) {
        let srv = std::sync::Arc::new(Self {
            arrivals: Default::default(),
            responses: std::sync::Mutex::new(responses).into(),
            delay_ms,
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv2 = srv.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let srv3 = srv2.clone();
                std::thread::spawn(move || srv3.handle(stream));
            }
        });
        (srv, port)
    }

    fn handle(&self, mut stream: std::net::TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until end of request headers (GET only).
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        self.arrivals.lock().unwrap().push(Instant::now());
        if self.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.delay_ms));
        }
        let req = String::from_utf8_lossy(&buf).into_owned();
        let path = req.split(' ').nth(1).unwrap_or("/").to_string();
        let responses = self.responses.lock().unwrap();
        let body = responses
            .iter()
            .find(|(prefix, _)| path == *prefix || path.starts_with(&format!("{prefix}?")))
            .map(|(_, b)| b.clone())
            .unwrap_or_else(|| "nope".to_string());
        drop(responses);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn flock_serializes_concurrent_calls() {
    // API-exempt path (no robots fetch); the mock is slow (<1s) so the
    // expensive-pause never fires — we are measuring lock serialization.
    let (srv, port) = RawServer::spawn(
        vec![("/w/api.php".to_string(), r#"{"ok":1}"#.to_string())],
        400,
    );

    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://127.0.0.1:{port}/w/api.php?x=1");
    let mut handles = Vec::new();
    for _ in 0..2 {
        let d = dir.path().to_path_buf();
        let url = url.clone();
        handles.push(std::thread::spawn(move || {
            run(isolated(&d)
                .arg(&url)
                .arg("--contact-email")
                .arg("t@example.org")
                .arg("--max-time")
                .arg("30"))
        }));
    }
    for h in handles {
        let out = h.join().unwrap();
        assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    }

    let arrivals = srv.arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 2, "both processes reached the server");
    // Serialization via state.lock: the second arrival must postdate the
    // first by at least the mock's 400ms response time.
    assert!(
        arrivals[1].duration_since(arrivals[0]) >= Duration::from_millis(380),
        "gap was {:?}",
        arrivals[1].duration_since(arrivals[0])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn robots_first_call_paced() {
    // Instant-responding mocks: a slow mock response would fake the arrival
    // gap. Assert robots-arrival → target-arrival ≥ the 250ms floor.
    let (srv, port) = RawServer::spawn(
        vec![
            (
                "/robots.txt".to_string(),
                "User-agent: *\nDisallow:\n".to_string(),
            ),
            ("/wiki/x".to_string(), "body".to_string()),
        ],
        0,
    );

    let dir = tempfile::tempdir().unwrap();
    let out = run(isolated(dir.path())
        .arg(format!("http://127.0.0.1:{port}/wiki/x"))
        .arg("--contact-email")
        .arg("t@example.org"));
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));

    let arrivals = srv.arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 2, "robots + target");
    let gap = arrivals[1].duration_since(arrivals[0]);
    assert!(gap >= Duration::from_millis(240), "gap was {gap:?}");
}

// ------------------------------------------------- per-host concurrency ---
// 2.1: non-Wikimedia hosts get per-host locks and per-host pacing state;
// Wikimedia hosts keep machine-wide serialization. Process-based, since
// in-process sessions would share file locks and pass vacuously.

#[tokio::test(flavor = "multi_thread")]
async fn per_host_concurrency_processes() {
    // Two distinct hosts (localhost vs 127.0.0.1 — distinct host strings),
    // each with a 400ms API response. Two processes in parallel must
    // complete in less than the serialized lower bound (~1050ms under the
    // pre-2.1 global lock + 250ms floor).
    let (_srv1, port1) = RawServer::spawn(
        vec![("/w/api.php".to_string(), r#"{"ok":1}"#.to_string())],
        400,
    );
    let (_srv2, port2) = RawServer::spawn(
        vec![("/w/api.php".to_string(), r#"{"ok":2}"#.to_string())],
        400,
    );

    let dir = tempfile::tempdir().unwrap();
    let mut handles = Vec::new();
    for (host, port) in [("localhost", port1), ("127.0.0.1", port2)] {
        let d = dir.path().to_path_buf();
        handles.push(std::thread::spawn(move || {
            run(isolated(&d)
                .arg(format!("http://{host}:{port}/w/api.php?x=1"))
                .arg("--contact-email")
                .arg("t@example.org")
                .arg("--max-time")
                .arg("30"))
        }));
    }
    let t0 = Instant::now();
    for h in handles {
        let out = h.join().unwrap();
        assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(900),
        "different hosts must run concurrently, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn same_host_stays_serialized_processes() {
    // Same host, two processes: the per-host lock plus the 1000ms per-host
    // pacing floor keep arrivals apart.
    let (srv, port) = RawServer::spawn(
        vec![("/w/api.php".to_string(), r#"{"ok":1}"#.to_string())],
        400,
    );

    let dir = tempfile::tempdir().unwrap();
    let mut handles = Vec::new();
    for _ in 0..2 {
        let d = dir.path().to_path_buf();
        handles.push(std::thread::spawn(move || {
            run(isolated(&d)
                .arg(format!("http://127.0.0.1:{port}/w/api.php?x=1"))
                .arg("--contact-email")
                .arg("t@example.org")
                .arg("--max-time")
                .arg("30"))
        }));
    }
    for h in handles {
        let out = h.join().unwrap();
        assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    }

    let arrivals = srv.arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 2, "both processes reached the server");
    assert!(
        arrivals[1].duration_since(arrivals[0]) >= Duration::from_millis(380),
        "same-host arrivals were {arrivals:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wikimedia_hosts_stay_serialized() {
    // With the tighten-only test override, loopback hosts classify as
    // Wikimedia and share the machine-wide state.lock: a second process
    // times out against the lock while the first retains it.
    let (_srv_a, port_a) = RawServer::spawn(
        vec![("/w/api.php".to_string(), r#"{"ok":1}"#.to_string())],
        1500,
    );
    let (_srv_b, port_b) = RawServer::spawn(
        vec![("/w/api.php".to_string(), r#"{"ok":2}"#.to_string())],
        0,
    );

    let dir = tempfile::tempdir().unwrap();
    let d1 = dir.path().to_path_buf();
    let a = std::thread::spawn(move || {
        let mut c = isolated(&d1);
        c.arg(format!("http://127.0.0.1:{port_a}/w/api.php?x=1"))
            .arg("--contact-email")
            .arg("t@example.org")
            .arg("--max-time")
            .arg("30")
            .env("WM_FETCH_WMF_TEST_FORCE_WIKIMEDIA", "1");
        run(&mut c)
    });
    std::thread::sleep(Duration::from_millis(150));
    let d2 = dir.path().to_path_buf();
    let b = std::thread::spawn(move || {
        let mut c = isolated(&d2);
        c.arg(format!("http://127.0.0.1:{port_b}/w/api.php?x=1"))
            .arg("--contact-email")
            .arg("t@example.org")
            .arg("--max-time")
            .arg("0.5")
            .env("WM_FETCH_WMF_TEST_FORCE_WIKIMEDIA", "1");
        run(&mut c)
    });

    let out_a = a.join().unwrap();
    assert_eq!(code(&out_a), 0, "stderr: {}", stderr(&out_a));
    let out_b = b.join().unwrap();
    assert_eq!(code(&out_b), 1, "second process must time out on the lock");
    assert!(
        stderr(&out_b).contains("lock wait would exceed --max-time"),
        "stderr: {}",
        stderr(&out_b)
    );
}

// --------------------------------------------------------------- AC.11 ---

#[test]
fn exit_codes_documented() {
    let out = run(bin().arg("--help"));
    assert_eq!(code(&out), 0);
    let help = stdout(&out);
    for token in [
        "0  success",
        "1  transport",
        "2  usage",
        "3  policy refusal",
    ] {
        assert!(help.contains(token), "--help missing {token:?}:\n{help}");
    }
    // The three exit-3 causes must be enumerated.
    for cause in ["robots.txt disallow", "unreachable after cache", "cooldown"] {
        assert!(
            help.contains(cause),
            "--help missing exit-3 cause {cause:?}:\n{help}"
        );
    }
}
