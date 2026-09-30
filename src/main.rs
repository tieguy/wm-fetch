//! wm-fetch — fetch a URL with WMF automation policy enforced by
//! construction. Designed to be called by LLM agents: `wm-fetch <url>`.
//!
//! Exit codes:
//!   0  success (body on stdout)
//!   1  transport failure / HTTP ≥ 400 after retries / --max-time abort
//!   2  usage or configuration error (including: no operator contact
//!      configured and no --user-agent given)
//!   3  policy refusal: robots.txt disallow, robots.txt unreachable after
//!      cache fallback, or other-services 5xx cooldown

mod classify;
mod config;
mod http;
mod maxlag;
mod pacing;
mod robots;
mod ua;

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use config::CliOverrides;
use http::{Fail, Session, EXIT_FAIL, EXIT_OK, EXIT_POLICY, EXIT_USAGE};

#[derive(Parser, Debug)]
#[command(
    name = "wm-fetch",
    version,
    about = "Fetch a URL with Wikimedia automation policy enforced (User-Agent, maxlag, pacing, robots.txt)",
    long_about = "wm-fetch — fetch a URL with Wikimedia automation policy enforced by construction.\n\n\
        Every request carries a policy-compliant User-Agent with the operator's \
        contact info, Action API calls carry maxlag, 429/503 responses are retried \
        with Retry-After backoff, requests are paced and serialized across \
        invocations, and robots.txt is honored (scoped: API endpoints are governed \
        by API:Etiquette instead — see the README).\n\n\
        Exit codes:\n  \
        0  success (body on stdout)\n  \
        1  transport failure / HTTP >= 400 after retries / --max-time abort\n  \
        2  usage or configuration error (no contact configured, no --user-agent)\n  \
        3  policy refusal: robots.txt disallow; robots.txt unreachable after cache \
        fallback; other-services 5xx cooldown (gerrit/gitlab/phabricator/lists)"
)]
struct Cli {
    /// URL to fetch (http/https)
    url: Option<String>,

    /// Contact email for the User-Agent (WMF UA policy requires contact info)
    #[arg(long)]
    contact_email: Option<String>,

    /// Contact page (e.g. on-wiki user page) for the User-Agent
    #[arg(long)]
    contact_page: Option<String>,

    /// Client name in the User-Agent (default: wm-fetch-bot)
    #[arg(long)]
    client_name: Option<String>,

    /// Replace the constructed User-Agent entirely. A custom UA must itself
    /// satisfy the UA policy (contact info in parentheses) — that
    /// obligation is yours.
    #[arg(long)]
    user_agent: Option<String>,

    /// Whole-request budget in seconds, including pacing and lock waits
    #[arg(long)]
    max_time: Option<f64>,

    /// Connect timeout in seconds
    #[arg(long)]
    connect_timeout: Option<f64>,

    /// Maximum redirect hops to follow
    #[arg(long, value_name = "N")]
    max_redirs: Option<usize>,

    /// Retry attempts for 429/503/maxlag (0 = one attempt, no retries)
    #[arg(long)]
    retries: Option<usize>,

    /// maxlag seconds appended to Action API URLs lacking one (default 5)
    #[arg(long)]
    maxlag: Option<u64>,

    /// Path to a config file (default: ~/.config/wm-fetch/config.toml)
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Write a commented config template (if none exists) and exit
    #[arg(long)]
    init: bool,

    /// Print the effective config and the User-Agent it would send, then exit
    #[arg(long)]
    print_config: bool,
}

fn overrides_from(cli: &Cli) -> CliOverrides {
    CliOverrides {
        client_name: cli.client_name.clone(),
        contact_email: cli.contact_email.clone(),
        contact_page: cli.contact_page.clone(),
        user_agent: cli.user_agent.clone(),
        maxlag: cli.maxlag,
        retries: cli.retries,
        max_time: cli.max_time,
        connect_timeout: cli.connect_timeout,
        max_redirs: cli.max_redirs,
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = run(&cli);
    ExitCode::from(code as u8)
}

fn warn(msg: &str) {
    eprintln!("wm-fetch: {msg}");
}

fn run(cli: &Cli) -> i32 {
    // WM_FETCH_CONFIG from the flag (or env) is honored by config::load.
    if let Some(p) = &cli.config {
        std::env::set_var("WM_FETCH_CONFIG", p);
    }

    let cfg = match config::load(&overrides_from(cli)) {
        Ok(c) => c,
        Err(e) => {
            warn(&format!("{e:#}"));
            return EXIT_USAGE;
        }
    };

    if cli.init {
        return do_init();
    }

    let effective_ua = cfg.user_agent.clone().unwrap_or_else(|| {
        ua::build(
            &cfg.client_name,
            cfg.contact_page.as_deref(),
            cfg.contact_email.as_deref(),
        )
    });

    if cli.print_config {
        println!("client_name      = {}", cfg.client_name);
        println!(
            "contact_email    = {}",
            cfg.contact_email.as_deref().unwrap_or("(unset)")
        );
        println!(
            "contact_page     = {}",
            cfg.contact_page.as_deref().unwrap_or("(unset)")
        );
        println!("user_agent       = {effective_ua}");
        println!("maxlag           = {}", cfg.maxlag);
        println!("retries          = {}", cfg.max_retries);
        println!("max_time         = {}s", cfg.max_time_secs);
        println!("connect_timeout  = {}s", cfg.connect_timeout_secs);
        println!("max_redirs       = {}", cfg.max_redirs);
        if cfg.user_agent.is_none() && cfg.contact_email.is_none() && cfg.contact_page.is_none() {
            println!(
                "status           = REFUSES TO FETCH: no contact configured \
                 (WMF User-Agent policy). Run `wm-fetch --init`."
            );
        }
        return EXIT_OK;
    }

    let Some(url_raw) = cli.url.clone() else {
        warn("usage: wm-fetch <url> [options]");
        return EXIT_USAGE;
    };

    // Fail-closed contact gate: never send an anonymous request. This is
    // the fix for the "fork ships the upstream author's contact" bug — a
    // fresh fork refuses to run until the operator configures their own.
    if cfg.user_agent.is_none() && cfg.contact_email.is_none() && cfg.contact_page.is_none() {
        warn(
            "no contact configured: the WMF User-Agent policy requires contact info in \
             every User-Agent. Set contact_email/contact_page (config file, \
             WM_FETCH_CONTACT_* env, or --contact-*), or pass --user-agent with your \
             own registered agent. Run `wm-fetch --init` to scaffold a config.",
        );
        return EXIT_USAGE;
    }

    if let Some(custom) = &cfg.user_agent {
        if !ua::has_contact_group(custom) {
            warn(
                "custom --user-agent carries no parenthesized contact group; the UA \
                 policy expects one. Sending it anyway — the obligation is yours.",
            );
        }
    }

    // maxlag injection happens on the Action API URL before fetching.
    let injected = maxlag::inject(&url_raw, cfg.maxlag);
    let parsed = match url::Url::parse(&injected) {
        Ok(u) => u,
        Err(e) => {
            warn(&format!("invalid URL {url_raw:?}: {e}"));
            return EXIT_USAGE;
        }
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        warn(&format!(
            "unsupported scheme {:?} (http/https only)",
            parsed.scheme()
        ));
        return EXIT_USAGE;
    }

    let state_dir = config::state_dir();
    let mut session = match Session::new(cfg, effective_ua, state_dir) {
        Ok(s) => s,
        Err(Fail::Fatal(m)) | Err(Fail::Budget(m)) => {
            warn(&m);
            return EXIT_FAIL;
        }
        Err(Fail::Policy(m)) => {
            warn(&m);
            return EXIT_POLICY;
        }
    };

    match session.fetch(&parsed) {
        Ok(final_resp) => {
            let mut out = std::io::stdout();
            let _ = out.write_all(&final_resp.body);
            let _ = out.flush();
            if final_resp.failure || final_resp.status >= 400 {
                if final_resp.status < 400 {
                    warn(&format!("HTTP {} (retries exhausted)", final_resp.status));
                } else {
                    warn(&format!("HTTP {}", final_resp.status));
                }
                return EXIT_FAIL;
            }
            EXIT_OK
        }
        Err(Fail::Fatal(m)) | Err(Fail::Budget(m)) => {
            warn(&m);
            EXIT_FAIL
        }
        Err(Fail::Policy(m)) => {
            warn(&m);
            EXIT_POLICY
        }
    }
}

fn do_init() -> i32 {
    let path = config::config_path();
    if path.exists() {
        println!("config already exists: {}", path.display());
        return EXIT_OK;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn(&format!("creating {}: {e}", parent.display()));
            return EXIT_FAIL;
        }
    }
    match std::fs::write(&path, config::INIT_TEMPLATE) {
        Ok(()) => {
            println!("wrote {}", path.display());
            println!("Edit it to set contact_email or contact_page, then fetch away.");
            EXIT_OK
        }
        Err(e) => {
            warn(&format!("writing {}: {e}", path.display()));
            EXIT_FAIL
        }
    }
}
