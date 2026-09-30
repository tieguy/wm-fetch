//! Live tests against real Wikimedia infrastructure. NOT run in CI.
//!
//! Run them with your contact configured:
//!   WM_FETCH_LIVE_CONTACT="you@example.org" cargo test --release -- --ignored
//!
//! These ports the v1 smoke test legs: siteinfo round trip; maxlag provably
//! reaching the API (an invalid maxlag=abc comes back as the API's own
//! badinteger error naming the parameter); and a live robots.txt refusal.

use std::process::Command;

fn contact() -> Option<String> {
    std::env::var("WM_FETCH_LIVE_CONTACT")
        .ok()
        .filter(|s| !s.is_empty())
}

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_wm-fetch"));
    let dir = tempfile::tempdir().unwrap();
    c.env("WM_FETCH_STATE_DIR", dir.keep());
    c.env("WM_FETCH_CONFIG", "/nonexistent-wm-fetch-live-test.toml");
    c
}

fn run(url: &str) -> (i32, String, String) {
    let contact = contact().expect("set WM_FETCH_LIVE_CONTACT=you@example.org to run live tests");
    let out = bin()
        .arg(url)
        .arg("--contact-email")
        .arg(contact)
        .arg("--max-time")
        .arg("90") // live legs can be slow; still bounded
        .output()
        .expect("running wm-fetch");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
#[ignore = "hits real Wikimedia infrastructure; needs WM_FETCH_LIVE_CONTACT"]
fn live_siteinfo() {
    let (code, body, stderr) = run(
        "https://en.wikipedia.org/w/api.php?action=query&meta=siteinfo&format=json&formatversion=2",
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!(
            "siteinfo is not JSON ({e}): body starts {}",
            &body[..body.len().min(200)]
        )
    });
    assert_eq!(
        v["query"]["general"]["servername"].as_str(),
        Some("en.wikipedia.org")
    );
}

#[test]
#[ignore = "hits real Wikimedia infrastructure; needs WM_FETCH_LIVE_CONTACT"]
fn live_maxlag_reaches_api() {
    // An INVALID maxlag must come back as the API's own badinteger error
    // NAMING the parameter — proving our maxlag reaches the API. (Our
    // injector leaves an explicit maxlag=abc untouched.) Note: api.php
    // reports parameter errors as HTTP 200 with error JSON, so this is a
    // success-path fetch whose body we inspect.
    let (code, body, stderr) =
        run("https://en.wikipedia.org/w/api.php?action=query&meta=siteinfo&format=json&maxlag=abc");
    assert_eq!(code, 0, "stderr: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", &body[..body.len().min(200)]));
    assert_eq!(
        v["error"]["code"].as_str(),
        Some("badinteger"),
        "body: {body}"
    );
    assert!(
        v["error"]["info"]
            .as_str()
            .unwrap_or_default()
            .contains("maxlag"),
        "error does not name maxlag: {body}"
    );
}

#[test]
#[ignore = "hits real Wikimedia infrastructure; needs WM_FETCH_LIVE_CONTACT"]
fn live_special_refused() {
    // en.wikipedia.org/robots.txt disallows /wiki/Special: — the tool must
    // refuse with exit 3, having fetched (and cached) only robots.txt.
    let (code, body, stderr) = run("https://en.wikipedia.org/wiki/Special:Export");
    assert_eq!(code, 3, "stderr: {stderr}");
    assert!(body.is_empty(), "refusal must not print a body: {body}");
    assert!(
        stderr.contains("disallows") && stderr.contains("Special:Export"),
        "stderr: {stderr}"
    );
}
