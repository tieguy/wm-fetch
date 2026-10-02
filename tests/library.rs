//! In-process library tests (AC.3): raw wire capture, SSRF refusal, body
//! cap, contact gate, record-only robots. Offline (wiremock). Timing is
//! untouched here — process-based serialization tests live in
//! integration.rs.
//!
//! The blocking reqwest client owns a tokio runtime which must not be
//! dropped inside an async context, so every session is driven inside
//! `spawn_blocking`.

use std::path::PathBuf;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use wm_fetch::config::Config;
use wm_fetch::http::{decode_body, Fail, Final, RobotsMode, Session, SessionOptions};
use wm_fetch::robots::{Report, ReportVerdict};

fn cfg() -> Config {
    Config {
        contact_email: Some("lib-tester@example.org".into()),
        max_retries: 0,
        ..Config::default()
    }
}

fn state_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wmfetch-libtest-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn allow_robots(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow:\n"))
        .mount(server)
        .await;
}

fn gzipped(payload: &str) -> Vec<u8> {
    use std::io::Read;
    let mut gz = flate2::read::GzEncoder::new(
        std::io::Cursor::new(payload.as_bytes().to_vec()),
        flate2::Compression::default(),
    );
    let mut wire = Vec::new();
    gz.read_to_end(&mut wire).unwrap();
    wire
}

/// Run `f` on a blocking thread (where the session and its inner runtime
/// may be created and dropped).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("blocking task joins")
}

#[tokio::test]
async fn library_refuses_without_contact() {
    let err = blocking(
        move || match Session::connect(Config::default(), state_dir("nocontact")) {
            Err(e) => e,
            Ok(_) => panic!("contactless session must not build"),
        },
    )
    .await;
    match err {
        Fail::Config(msg) => assert!(msg.contains("no contact configured"), "{msg}"),
        other => panic!("expected Fail::Config, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_capture_gzip_bytes() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    let wire = gzipped("hello wire capture");
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-encoding", "gzip")
                .set_body_bytes(wire.clone()),
        )
        .mount(&server)
        .await;

    let options = SessionOptions {
        raw_capture: true,
        ..SessionOptions::default()
    };
    let url = url::Url::parse(&format!("{}/page", server.uri())).unwrap();
    let final_resp: Final = blocking(move || {
        let mut session = Session::connect_with(cfg(), state_dir("rawcap"), options).unwrap();
        session.fetch(&url).unwrap()
    })
    .await;

    // Wire bytes, not decoded text.
    assert_eq!(final_resp.body, wire);
    assert_eq!(
        decode_body(
            &final_resp.hops.last().unwrap().response_headers,
            &final_resp.body
        )
        .unwrap(),
        b"hello wire capture".to_vec()
    );
    // Per-hop record: status, response headers, request UA.
    assert_eq!(final_resp.hops.len(), 1);
    let hop = &final_resp.hops[0];
    assert_eq!(hop.status, 200);
    assert_eq!(
        hop.response_headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok()),
        Some("gzip")
    );
    assert!(hop
        .request_headers
        .iter()
        .any(|(k, v)| k == "user-agent" && v.contains("lib-tester@example.org")));
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_private_targets() {
    let server = MockServer::start().await;
    // The loopback literal is refused before any request leaves.
    Mock::given(method("GET"))
        .and(path("/wiki/x"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let options = SessionOptions {
        refuse_internal_addresses: true,
        // Record-only so the fail-closed robots.txt path (which would
        // surface as "robots.txt unreachable" after the guarded resolver
        // refuses the robots fetch for localhost) does not mask the SSRF
        // error on the target request itself.
        robots_mode: RobotsMode::RecordOnly,
        ..SessionOptions::default()
    };
    let loopback = url::Url::parse(&format!("{}/wiki/x", server.uri())).unwrap();
    let results = blocking(move || {
        let mut session = Session::connect_with(cfg(), state_dir("ssrf"), options).unwrap();
        let private = url::Url::parse("http://10.1.2.3/wiki/x").unwrap();
        let localhost = url::Url::parse(&format!(
            "http://localhost:{}/wiki/x",
            loopback.port().unwrap()
        ))
        .unwrap();
        vec![
            session.fetch(&loopback),
            session.fetch(&private),
            session.fetch(&localhost),
        ]
    })
    .await;
    // IP literal: loopback.
    match &results[0] {
        Err(Fail::Policy(msg)) => assert!(msg.contains("SSRF"), "{msg}"),
        other => panic!("expected policy refusal, got {other:?}"),
    }
    // IP literal: RFC1918 private (pre-flight).
    match &results[1] {
        Err(Fail::Policy(msg)) => assert!(msg.contains("SSRF"), "{msg}"),
        other => panic!("expected policy refusal, got {other:?}"),
    }
    // DNS name resolving to loopback: refused by the guarded resolver.
    match &results[2] {
        Err(Fail::Fatal(msg)) => assert!(msg.contains("SSRF"), "{msg}"),
        other => panic!("expected guarded-resolver refusal, got {other:?}"),
    }
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_redirect_to_loopback() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/from"))
        .respond_with(ResponseTemplate::new(301).insert_header("Location", "http://127.0.0.1:9/to"))
        .mount(&server)
        .await;

    let options = SessionOptions {
        refuse_internal_addresses: true,
        ..SessionOptions::default()
    };
    let url = url::Url::parse(&format!("{}/from", server.uri())).unwrap();
    let result = blocking(move || {
        let mut session = Session::connect_with(cfg(), state_dir("ssrfredir"), options).unwrap();
        session.fetch(&url)
    })
    .await;
    match result {
        Err(Fail::Policy(msg)) => assert!(msg.contains("SSRF"), "{msg}"),
        other => panic!("expected policy refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn body_cap_enforced() {
    let server = MockServer::start().await;
    allow_robots(&server).await;
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_string("0123456789"))
        .mount(&server)
        .await;

    let url = url::Url::parse(&format!("{}/big", server.uri())).unwrap();

    let capped = {
        let options = SessionOptions {
            max_body_bytes: Some(5),
            ..SessionOptions::default()
        };
        let url = url.clone();
        blocking(move || {
            let mut session = Session::connect_with(cfg(), state_dir("cap"), options).unwrap();
            session.fetch(&url)
        })
        .await
    };
    match capped {
        Err(Fail::TooLarge { limit, observed }) => {
            assert_eq!(limit, 5);
            assert!(observed >= 5, "observed {observed}");
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }

    // Cap above the body: fetch succeeds.
    let uncapped = {
        let options = SessionOptions {
            max_body_bytes: Some(50),
            ..SessionOptions::default()
        };
        blocking(move || {
            let mut session = Session::connect_with(cfg(), state_dir("cap2"), options).unwrap();
            session.fetch(&url)
        })
        .await
    };
    assert_eq!(uncapped.unwrap().body, b"0123456789".to_vec());
}

#[tokio::test(flavor = "multi_thread")]
async fn robots_record_only_fetches_and_records() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /wiki/x\n"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/x"))
        .respond_with(ResponseTemplate::new(200).set_body_string("fetched anyway"))
        .mount(&server)
        .await;

    // Record-only: fetch proceeds, verdict recorded.
    let options = SessionOptions {
        robots_mode: RobotsMode::RecordOnly,
        ..SessionOptions::default()
    };
    let url = url::Url::parse(&format!("{}/wiki/x", server.uri())).unwrap();
    let final_resp: Final = blocking(move || {
        let mut session = Session::connect_with(cfg(), state_dir("reconly"), options).unwrap();
        session.fetch(&url).unwrap()
    })
    .await;
    assert_eq!(final_resp.body, b"fetched anyway".to_vec());
    assert_eq!(
        final_resp.robots,
        Some(Report {
            verdict: ReportVerdict::Disallowed,
            matched_rules: vec!["disallow: /wiki/x".to_string()],
        })
    );

    // Default (enforce): refusal. (A second mock for the same path with
    // expect(0) would conflict with the one above; a fresh server keeps
    // the counts clean.)
    let server2 = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /wiki/x\n"),
        )
        .mount(&server2)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/x"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server2)
        .await;
    let url2 = url::Url::parse(&format!("{}/wiki/x", server2.uri())).unwrap();
    let result = blocking(move || {
        let mut session =
            Session::connect_with(cfg(), state_dir("enforce"), SessionOptions::default()).unwrap();
        session.fetch(&url2)
    })
    .await;
    match result {
        Err(Fail::Policy(msg)) => assert!(msg.contains("disallows /wiki/x"), "{msg}"),
        other => panic!("expected policy refusal, got {other:?}"),
    }
    server2.verify().await;
}

// Staged addition to wm-fetch tests/library.rs: regression test for the
// robots.txt redirect-hop gate (Phase 2 review round 2).
#[tokio::test(flavor = "multi_thread")]
async fn robots_redirect_hop_is_gated() {
    // robots.txt redirects to a non-public IP literal. With the SSRF
    // guard enabled, the redirect hop is refused before any connection —
    // proving fetch_robots's loop runs gate_hop (regression test for the
    // Phase 2 review finding: removing the gate must fail this test).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("Location", "http://10.255.255.1/robots.txt"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/x"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x"))
        .expect(0)
        .mount(&server)
        .await;

    let options = SessionOptions {
        refuse_internal_addresses: true,
        ..SessionOptions::default()
    };
    let url = url::Url::parse(&format!("{}/wiki/x", server.uri())).unwrap();
    let result = blocking(move || {
        let mut session = Session::connect_with(cfg(), state_dir("robotsredir"), options).unwrap();
        session.fetch(&url)
    })
    .await;
    match result {
        Err(Fail::Policy(msg)) => assert!(msg.contains("SSRF"), "{msg}"),
        other => panic!("expected the gated robots redirect to be refused, got {other:?}"),
    }
    server.verify().await;
}
