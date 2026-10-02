//! wm-fetch — fetch a URL with WMF automation policy enforced by
//! construction, as a library.
//!
//! The binary (`wm-fetch <url>`) is a thin CLI over this crate. Library
//! consumers get the same policy spine: every request carries a
//! policy-compliant User-Agent with the operator's contact, the
//! constructor refuses to build without contact, Action API calls carry
//! maxlag, 429/503 responses are retried with Retry-After backoff,
//! requests are paced and serialized across invocations, and robots.txt is
//! honored (scoped; API endpoints are governed by API:Etiquette instead).
//!
//! Library-only affordances (all default-off, CLI behaviour unchanged):
//! - [`http::SessionOptions::raw_capture`] — wire-byte bodies plus per-hop
//!   header records, for WARC-accurate archiving
//!   ([`http::decode_body`] decodes).
//! - [`http::SessionOptions::refuse_internal_addresses`] — SSRF guard on
//!   IP literals and resolved addresses (ported from SP42).
//! - [`http::SessionOptions::max_body_bytes`] — per-response body cap.
//! - [`http::SessionOptions::robots_mode`] — record-only robots: consult,
//!   pace, record the verdict, fetch anyway. A downstream tool owns its
//!   robots posture with recording built in; this crate's default remains
//!   enforcement.

pub mod classify;
pub mod config;
pub mod http;
pub mod maxlag;
pub mod pacing;
pub mod robots;
pub mod ssrf;
pub mod ua;

pub use http::{decode_body, Fail, Final, Hop, RobotsMode, RobotsReport, Session, SessionOptions};
