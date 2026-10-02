//! Configuration: TOML file < environment < CLI flags.
//!
//! Compliance invariants are NOT configurable: the UA is always constructed
//! with operator contact (or explicitly replaced via `--user-agent`, which
//! carries the caller's own obligation), robots enforcement and pacing
//! floors are always on. Config changes identity or timing, never compliance.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_MAXLAG: u64 = 5;
pub const DEFAULT_RETRIES: usize = 4;
pub const DEFAULT_MAX_TIME: f64 = 60.0;
pub const DEFAULT_CONNECT_TIMEOUT: f64 = 10.0;
pub const DEFAULT_MAX_REDIRS: usize = 3;

#[derive(Debug, Clone)]
pub struct Config {
    pub client_name: String,
    pub contact_email: Option<String>,
    pub contact_page: Option<String>,
    pub user_agent: Option<String>,
    pub maxlag: u64,
    pub max_retries: usize,
    pub max_time_secs: f64,
    pub connect_timeout_secs: f64,
    pub max_redirs: usize,
    /// Machine-wide concurrency cap for non-Wikimedia hosts (the slot-pool
    /// size). Wikimedia hosts stay machine-wide serialized regardless.
    pub global_concurrency: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_name: crate::ua::DEFAULT_CLIENT_NAME.to_string(),
            contact_email: None,
            contact_page: None,
            user_agent: None,
            maxlag: DEFAULT_MAXLAG,
            max_retries: DEFAULT_RETRIES,
            max_time_secs: DEFAULT_MAX_TIME,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT,
            max_redirs: DEFAULT_MAX_REDIRS,
            global_concurrency: crate::pacing::DEFAULT_GLOBAL_CONCURRENCY,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    client_name: Option<String>,
    contact_email: Option<String>,
    contact_page: Option<String>,
    user_agent: Option<String>,
    maxlag: Option<u64>,
    retries: Option<usize>,
    max_time: Option<f64>,
    connect_timeout: Option<f64>,
    max_redirs: Option<usize>,
    global_concurrency: Option<usize>,
}

/// CLI overrides (all optional; highest precedence).
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub client_name: Option<String>,
    pub contact_email: Option<String>,
    pub contact_page: Option<String>,
    pub user_agent: Option<String>,
    pub maxlag: Option<u64>,
    pub retries: Option<usize>,
    pub max_time: Option<f64>,
    pub connect_timeout: Option<f64>,
    pub max_redirs: Option<usize>,
    pub global_concurrency: Option<usize>,
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("WM_FETCH_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("wm-fetch")
        .join("config.toml")
}

pub fn state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("WM_FETCH_STATE_DIR") {
        return PathBuf::from(p);
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("wm-fetch")
}

/// Load effective config from the real config path and real environment.
pub fn load(cli: &CliOverrides) -> Result<Config> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    load_from(&config_path(), &env, cli)
}

/// Pure loader: file at `path` (missing = no layer, never an error; malformed
/// = error), then `env` pairs (`WM_FETCH_*`), then CLI overrides. Pure so the
/// precedence tests don't mutate global state.
pub fn load_from(path: &Path, env: &[(String, String)], cli: &CliOverrides) -> Result<Config> {
    let mut cfg = Config::default();

    if path.is_file() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let f: FileConfig = toml::from_str(&raw)
            .with_context(|| format!("parsing config file {} (TOML)", path.display()))?;
        cfg.client_name = f.client_name.unwrap_or(cfg.client_name);
        cfg.contact_email = f.contact_email.or(cfg.contact_email);
        cfg.contact_page = f.contact_page.or(cfg.contact_page);
        cfg.user_agent = f.user_agent.or(cfg.user_agent);
        cfg.maxlag = f.maxlag.unwrap_or(cfg.maxlag);
        cfg.max_retries = f.retries.unwrap_or(cfg.max_retries);
        cfg.max_time_secs = f.max_time.unwrap_or(cfg.max_time_secs);
        cfg.connect_timeout_secs = f.connect_timeout.unwrap_or(cfg.connect_timeout_secs);
        cfg.max_redirs = f.max_redirs.unwrap_or(cfg.max_redirs);
        cfg.global_concurrency = f.global_concurrency.unwrap_or(cfg.global_concurrency);
    }

    let get = |k: &str| -> Option<String> {
        env.iter()
            .find(|(ek, _)| ek == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    if let Some(v) = get("WM_FETCH_CLIENT_NAME") {
        cfg.client_name = v;
    }
    if let Some(v) = get("WM_FETCH_CONTACT_EMAIL") {
        cfg.contact_email = Some(v);
    }
    if let Some(v) = get("WM_FETCH_CONTACT_PAGE") {
        cfg.contact_page = Some(v);
    }
    if let Some(v) = get("WM_FETCH_USER_AGENT") {
        cfg.user_agent = Some(v);
    }
    let env_num = |k: &str, setter: &mut dyn FnMut(f64)| -> Result<()> {
        if let Some(v) = get(k) {
            let n: f64 = v
                .parse()
                .with_context(|| format!("environment variable {k}={v:?} is not a number"))?;
            setter(n);
        }
        Ok(())
    };
    env_num("WM_FETCH_MAXLAG", &mut |n| cfg.maxlag = n as u64)?;
    env_num("WM_FETCH_RETRIES", &mut |n| cfg.max_retries = n as usize)?;
    env_num("WM_FETCH_MAX_TIME", &mut |n| cfg.max_time_secs = n)?;
    env_num("WM_FETCH_CONNECT_TIMEOUT", &mut |n| {
        cfg.connect_timeout_secs = n
    })?;
    env_num("WM_FETCH_MAX_REDIRS", &mut |n| cfg.max_redirs = n as usize)?;
    env_num("WM_FETCH_GLOBAL_CONCURRENCY", &mut |n| {
        cfg.global_concurrency = n as usize
    })?;

    let CliOverrides {
        client_name,
        contact_email,
        contact_page,
        user_agent,
        maxlag,
        retries,
        max_time,
        connect_timeout,
        max_redirs,
        global_concurrency,
    } = cli;
    if let Some(v) = client_name {
        cfg.client_name = v.clone();
    }
    if let Some(v) = contact_email {
        cfg.contact_email = Some(v.clone());
    }
    if let Some(v) = contact_page {
        cfg.contact_page = Some(v.clone());
    }
    if let Some(v) = user_agent {
        cfg.user_agent = Some(v.clone());
    }
    if let Some(v) = maxlag {
        cfg.maxlag = *v;
    }
    if let Some(v) = retries {
        cfg.max_retries = *v;
    }
    if let Some(v) = max_time {
        cfg.max_time_secs = *v;
    }
    if let Some(v) = connect_timeout {
        cfg.connect_timeout_secs = *v;
    }
    if let Some(v) = max_redirs {
        cfg.max_redirs = *v;
    }
    if let Some(v) = global_concurrency {
        cfg.global_concurrency = (*v).max(1);
    }

    // Empty/whitespace contact strings mean "unset" at every layer (the env
    // layer filters them on read; normalize file and flag layers the same
    // way) — otherwise an empty `--contact-email "$VAR"` with an unset
    // shell variable would slip past the fail-closed contact gate and send
    // an effectively anonymous User-Agent.
    let non_empty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
    cfg.contact_email = non_empty(cfg.contact_email);
    cfg.contact_page = non_empty(cfg.contact_page);
    cfg.user_agent = non_empty(cfg.user_agent);

    Ok(cfg)
}

pub const INIT_TEMPLATE: &str = r##"# wm-fetch configuration.
#
# At least one contact is REQUIRED before fetching, per the WMF User-Agent
# policy: https://foundation.wikimedia.org/wiki/Policy:Wikimedia_Foundation_User-Agent_Policy
# The tool refuses to run (exit 2) rather than send an anonymous request.
#
#contact_email = "you@example.org"
#contact_page = "https://en.wikipedia.org/wiki/User:YourName"

# Optional identity/timing knobs (defaults shown). Compliance behavior
# (User-Agent construction, robots.txt enforcement, pacing floors) is not
# configurable — see the README.
#client_name = "wm-fetch-bot"
#retries = 4
#max_time = 60
#connect_timeout = 10
#max_redirs = 3
#maxlag = 5
# Machine-wide concurrency cap for non-Wikimedia hosts (Wikimedia hosts stay
# machine-wide serialized regardless). Compliance floors are not configurable.
#global_concurrency = 8
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn no_env() -> Vec<(String, String)> {
        Vec::new()
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults() {
        let c = load_from(
            Path::new("/nonexistent"),
            &no_env(),
            &CliOverrides::default(),
        )
        .unwrap();
        assert_eq!(c.client_name, "wm-fetch-bot");
        assert_eq!(c.maxlag, 5);
        assert_eq!(c.max_retries, 4);
        assert_eq!(c.max_time_secs, 60.0);
        assert_eq!(c.connect_timeout_secs, 10.0);
        assert_eq!(c.max_redirs, 3);
        assert!(c.contact_email.is_none());
    }

    #[test]
    fn config_precedence_flag_env_file_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        writeln!(
            f,
            r#"contact_email = "file@example.org"
client_name = "filebot"
max_time = 30"#
        )
        .unwrap();
        drop(f);

        // file layer only
        let c = load_from(&cfg_path, &no_env(), &CliOverrides::default()).unwrap();
        assert_eq!(c.contact_email.as_deref(), Some("file@example.org"));
        assert_eq!(c.client_name, "filebot");
        assert_eq!(c.max_time_secs, 30.0);

        // env beats file
        let c = load_from(
            &cfg_path,
            &env(&[
                ("WM_FETCH_CONTACT_EMAIL", "env@example.org"),
                ("WM_FETCH_MAX_TIME", "20"),
            ]),
            &CliOverrides::default(),
        )
        .unwrap();
        assert_eq!(c.contact_email.as_deref(), Some("env@example.org"));
        assert_eq!(c.max_time_secs, 20.0);
        assert_eq!(c.client_name, "filebot"); // untouched by env

        // flag beats env
        let cli = CliOverrides {
            contact_email: Some("flag@example.org".into()),
            max_time: Some(10.0),
            ..Default::default()
        };
        let c = load_from(
            &cfg_path,
            &env(&[("WM_FETCH_CONTACT_EMAIL", "env@example.org")]),
            &cli,
        )
        .unwrap();
        assert_eq!(c.contact_email.as_deref(), Some("flag@example.org"));
        assert_eq!(c.max_time_secs, 10.0);

        // bad env number → error naming the variable
        let err = load_from(
            &cfg_path,
            &env(&[("WM_FETCH_MAX_TIME", "abc")]),
            &CliOverrides::default(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("WM_FETCH_MAX_TIME"), "{err}");
    }

    #[test]
    fn config_parse_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("bad.toml");
        std::fs::write(&cfg_path, "contact_email = ").unwrap();
        let err = load_from(&cfg_path, &no_env(), &CliOverrides::default()).unwrap_err();
        assert!(format!("{err}").contains("parsing config file"), "{err}");
    }

    #[test]
    fn missing_config_file_is_not_an_error() {
        let c = load_from(
            Path::new("/nonexistent/wm-fetch/config.toml"),
            &no_env(),
            &CliOverrides::default(),
        )
        .unwrap();
        assert!(c.contact_email.is_none());
    }
}
