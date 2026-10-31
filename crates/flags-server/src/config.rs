//! Everything the process cannot infer, read once from the environment.
//!
//! A setting whose wrong value fails silently has no default: a wrong public
//! URL or platform URL does not crash, it advertises an audience nothing
//! accepts. A setting that is merely inconvenient to get wrong (bind address,
//! log format) gets one. A variable that is set but unparseable is an error
//! naming it, never a fallback.

use std::net::SocketAddr;

use anyhow::{anyhow, Context, Result};

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub db_max_connections: u32,
    pub run_migrations: bool,
    pub bind: SocketAddr,
    /// Public origin, e.g. `https://flags.savvagent.com`.
    pub public_url: String,
    /// Token audience; exactly the `resource_uri` registered at the platform.
    /// Defaults to `{public_url}/mcp`.
    pub resource_uri: String,
    pub platform_url: String,
    /// `sav_rs_…` (or the older `otto_rs_…`): this service's credential at the
    /// platform.
    pub introspection_secret: String,
    /// `sav_whsec_…` (or the older `otto_whsec_…`): verifies the platform's
    /// lifecycle webhooks.
    pub platform_webhook_secret: String,
    pub extra_allowed_hosts: Vec<String>,
    /// Old hostnames that only redirect (`FLAGS_REDIRECT_HOSTS`): every path
    /// but the SDK API answers a 308 to [`Config::public_url`] (#135). Bare
    /// lowercase hostnames, disjoint from the allowed hosts and the public host.
    pub redirect_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    /// The most evaluations answered to public client keys that are billed per
    /// app per hour (`FLAGS_CLIENT_EVAL_CEILING_PER_HOUR`); more are served and
    /// not billed (#143).
    pub client_eval_ceiling_per_hour: i64,
    /// Retired variables ([`RETIRED`]) that are set in the environment, to be
    /// warned about once logging is up. See [`Config::warn_retired`].
    pub retired_set: Vec<&'static str>,
    pub log_format: LogFormat,
}

/// Variables this server no longer reads, with what replaced them. Setting one
/// is harmless; [`Config::warn_retired`] says so at startup.
pub const RETIRED: &[(&str, &str)] = &[(
    "FLAGS_ENFORCE_QUOTAS",
    "MCP tools are free and never refused for billing; production evaluations are \
     metered and keep serving. Remove it from the environment",
)];

/// The retired variables `is_set` reports as present. Presence is what
/// matters, not the value: `FLAGS_ENFORCE_QUOTAS=false` does nothing either.
fn retired_set(is_set: impl Fn(&str) -> bool) -> Vec<&'static str> {
    RETIRED
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| is_set(name))
        .collect()
}

fn retired_warning(name: &str) -> String {
    let why = RETIRED
        .iter()
        .find(|(n, _)| *n == name)
        .map_or("", |(_, why)| why);
    format!("{name} is set but has no effect: it is retired. {why}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

/// Written by hand so a `{:?}` can never print a secret.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("public_url", &self.public_url)
            .field("resource_uri", &self.resource_uri)
            .field("platform_url", &self.platform_url)
            .field("retired_set", &self.retired_set)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let public_url = url_var("FLAGS_PUBLIC_URL")?;
        let resource_uri = match optional("FLAGS_RESOURCE_URI") {
            Some(v) => v,
            None => format!("{public_url}/mcp"),
        };
        let extra_allowed_hosts = list("FLAGS_ALLOWED_HOSTS");
        let redirect_hosts = redirect_hosts(
            &public_url,
            &extra_allowed_hosts,
            list("FLAGS_REDIRECT_HOSTS"),
        )?;
        Ok(Self {
            // `fly postgres attach` sets DATABASE_URL; FLAGS_DATABASE_URL wins
            // so a shell with another service's DATABASE_URL cannot point this
            // one at the wrong database by accident.
            database_url: optional("FLAGS_DATABASE_URL")
                .or_else(|| optional("DATABASE_URL"))
                .ok_or_else(|| {
                    anyhow!("FLAGS_DATABASE_URL (or DATABASE_URL) is required and not set")
                })?,
            db_max_connections: parse_var("FLAGS_DB_MAX_CONNECTIONS", "5", |s| {
                s.parse::<u32>()
                    .ok()
                    .filter(|n| *n >= 2)
                    .ok_or_else(|| anyhow!("a whole number of at least 2"))
            })?,
            run_migrations: parse_var("FLAGS_RUN_MIGRATIONS", "true", bool_value)?,
            bind: parse_var("FLAGS_BIND", "0.0.0.0:8080", |s| {
                s.parse()
                    .map_err(|_| anyhow!("a socket address like 0.0.0.0:8080"))
            })?,
            public_url,
            resource_uri,
            platform_url: url_var("FLAGS_PLATFORM_URL")?,
            introspection_secret: required("FLAGS_INTROSPECTION_SECRET")?,
            platform_webhook_secret: required("FLAGS_PLATFORM_WEBHOOK_SECRET")?,
            extra_allowed_hosts,
            redirect_hosts,
            allowed_origins: list("FLAGS_ALLOWED_ORIGINS"),
            client_eval_ceiling_per_hour: parse_var(
                "FLAGS_CLIENT_EVAL_CEILING_PER_HOUR",
                &flags_core::metering::DEFAULT_CLIENT_CEILING_PER_HOUR.to_string(),
                |s| {
                    s.parse::<i64>()
                        .ok()
                        .filter(|n| *n >= 0)
                        .ok_or_else(|| anyhow!("a whole number of at least 0"))
                },
            )?,
            retired_set: retired_set(|name| std::env::var_os(name).is_some()),
            log_format: parse_var("FLAGS_LOG_FORMAT", "text", |s| match s {
                "text" => Ok(LogFormat::Text),
                "json" => Ok(LogFormat::Json),
                _ => Err(anyhow!("text or json")),
            })?,
        })
    }

    /// Log a warning for each retired variable that is set, so an operator who
    /// still sets one learns it does nothing. Call once logging is up.
    pub fn warn_retired(&self) {
        for name in &self.retired_set {
            tracing::warn!(variable = name, "{}", retired_warning(name));
        }
    }

    /// Where an org upgrades its plan, named in plan-gate errors: the
    /// platform's billing settings. The one source for the MCP surface and
    /// the rollback checker.
    pub fn upgrade_url(&self) -> String {
        format!(
            "{}/settings/billing",
            self.platform_url.trim_end_matches('/')
        )
    }

    /// The public origin's host, plus any extras (e.g. the `*.fly.dev` name).
    pub fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = url::Url::parse(&self.public_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .into_iter()
            .collect();
        hosts.extend(self.extra_allowed_hosts.iter().cloned());
        hosts
    }

    /// A complete configuration with placeholder values, for tests.
    #[doc(hidden)]
    pub fn for_test() -> Self {
        Self {
            database_url: "postgres://localhost/test".into(),
            db_max_connections: 5,
            run_migrations: true,
            bind: "127.0.0.1:0".parse().unwrap(),
            public_url: "https://flags.example.com".into(),
            resource_uri: "https://flags.example.com/mcp".into(),
            platform_url: "https://platform.example.com".into(),
            introspection_secret: "otto_rs_test".into(),
            platform_webhook_secret: "otto_whsec_test".into(),
            extra_allowed_hosts: vec![],
            redirect_hosts: vec![],
            allowed_origins: vec![],
            client_eval_ceiling_per_hour: flags_core::metering::DEFAULT_CLIENT_CEILING_PER_HOUR,
            retired_set: vec![],
            log_format: LogFormat::Text,
        }
    }
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn required(name: &str) -> Result<String> {
    optional(name).ok_or_else(|| anyhow!("{name} is required and not set"))
}

fn url_var(name: &str) -> Result<String> {
    let v = required(name)?;
    let parsed = url::Url::parse(&v).with_context(|| format!("{name} is not a URL: {v:?}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(anyhow!("{name} must be an absolute http(s) URL, got {v:?}"));
    }
    Ok(v.trim_end_matches('/').to_string())
}

fn list(name: &str) -> Vec<String> {
    optional(name).map(|v| split_list(&v)).unwrap_or_default()
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether `h` is a bare lowercase hostname: dot-separated labels of
/// `a-z0-9-`, none empty or starting or ending with `-`. No scheme, port,
/// path, wildcard, uppercase or whitespace, so it compares equal to the host
/// of a request (which [`crate::redirect`] lowercases and strips of its port).
fn is_bare_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Validate `FLAGS_REDIRECT_HOSTS`: each entry a bare lowercase hostname, and
/// none the public host or one of `allowed` (a host cannot both serve /mcp and
/// redirect it). Refused at startup, naming the entry.
fn redirect_hosts(public_url: &str, allowed: &[String], hosts: Vec<String>) -> Result<Vec<String>> {
    let public_host = url::Url::parse(public_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string));
    for h in &hosts {
        if !is_bare_host(h) {
            return Err(anyhow!(
                "FLAGS_REDIRECT_HOSTS entry {h:?} is not a bare lowercase hostname \
                 (no scheme, port, path or wildcard)"
            ));
        }
        if public_host.as_deref() == Some(h.as_str()) {
            return Err(anyhow!(
                "FLAGS_REDIRECT_HOSTS lists {h:?}, which is the public host \
                 (FLAGS_PUBLIC_URL): it would redirect to itself"
            ));
        }
        if allowed.iter().any(|a| a.eq_ignore_ascii_case(h)) {
            return Err(anyhow!(
                "{h:?} is in both FLAGS_REDIRECT_HOSTS and FLAGS_ALLOWED_HOSTS: a host either \
                 serves /mcp or redirects it"
            ));
        }
    }
    Ok(hosts)
}

fn bool_value(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("true or false")),
    }
}

fn parse_var<T>(name: &str, default: &str, parse: impl Fn(&str) -> Result<T>) -> Result<T> {
    let raw = optional(name);
    let value = raw.as_deref().unwrap_or(default);
    parse(value).with_context(|| format!("{name}={value:?} is not valid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_hosts_start_with_the_public_host() {
        let mut c = Config::for_test();
        c.extra_allowed_hosts = vec!["otto-flags.fly.dev".into()];
        assert_eq!(
            c.allowed_hosts(),
            ["flags.example.com", "otto-flags.fly.dev"]
        );
    }

    /// The `[env]` values `fly.toml` deploys, read as `KEY = "value"` lines.
    fn fly_env(key: &str) -> String {
        let toml = include_str!("../../../fly.toml");
        let env = toml
            .split_once("\n[env]\n")
            .expect("fly.toml has an [env] table")
            .1;
        let env = env.split("\n[").next().unwrap();
        env.lines()
            .filter_map(|l| l.trim().split_once(" = "))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.trim_matches('"').to_string())
            .unwrap_or_else(|| panic!("fly.toml [env] does not set {key}"))
    }

    /// Rebrand phase 3 (#194) and #135: production serves
    /// flags.savvagent.com, and tokens are audienced for its /mcp. /mcp also
    /// accepts the fly.dev name; the legacy otto-flags.savvagent.com host only
    /// redirects (SDK API excepted).
    #[test]
    fn production_serves_the_savvagent_host_and_redirects_the_legacy_one() {
        let mut c = Config::for_test();
        c.public_url = fly_env("FLAGS_PUBLIC_URL");
        c.resource_uri = fly_env("FLAGS_RESOURCE_URI");
        c.extra_allowed_hosts = split_list(&fly_env("FLAGS_ALLOWED_HOSTS"));
        assert_eq!(c.public_url, "https://flags.savvagent.com");
        assert_eq!(c.resource_uri, "https://flags.savvagent.com/mcp");
        assert_eq!(
            c.allowed_hosts(),
            ["flags.savvagent.com", "otto-flags.fly.dev"]
        );
        // The deployed lists pass the startup validation.
        assert_eq!(
            redirect_hosts(
                &c.public_url,
                &c.extra_allowed_hosts,
                split_list(&fly_env("FLAGS_REDIRECT_HOSTS"))
            )
            .unwrap(),
            ["otto-flags.savvagent.com"]
        );
        assert_eq!(fly_env("FLAGS_PLATFORM_URL"), "https://savvagent.com");
    }

    fn hosts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn redirect_hosts_take_bare_lowercase_hostnames() {
        let ok = redirect_hosts(
            "https://flags.savvagent.com",
            &hosts(&["otto-flags.fly.dev"]),
            hosts(&["otto-flags.savvagent.com", "old"]),
        )
        .unwrap();
        assert_eq!(ok, ["otto-flags.savvagent.com", "old"]);
        for bad in [
            "https://otto-flags.savvagent.com",
            "otto-flags.savvagent.com:443",
            "otto-flags.savvagent.com/",
            "Otto-Flags.savvagent.com",
            "*.savvagent.com",
            "-a.example.com",
            "a..example.com",
            "a b",
            "otto-flags.savvagent.com.",
        ] {
            let e = redirect_hosts("https://flags.savvagent.com", &[], hosts(&[bad]))
                .unwrap_err()
                .to_string();
            assert!(e.contains("FLAGS_REDIRECT_HOSTS") && e.contains(bad), "{e}");
        }
    }

    #[test]
    fn a_redirect_host_may_not_also_serve_or_be_the_public_host() {
        let both = redirect_hosts(
            "https://flags.savvagent.com",
            &hosts(&["otto-flags.fly.dev", "otto-flags.savvagent.com"]),
            hosts(&["otto-flags.savvagent.com"]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            both.contains("both FLAGS_REDIRECT_HOSTS and FLAGS_ALLOWED_HOSTS"),
            "{both}"
        );
        let public = redirect_hosts(
            "https://flags.savvagent.com",
            &[],
            hosts(&["flags.savvagent.com"]),
        )
        .unwrap_err()
        .to_string();
        assert!(public.contains("public host"), "{public}");
    }

    #[test]
    fn the_client_ceiling_defaults_and_takes_zero_but_not_a_negative() {
        assert_eq!(Config::for_test().client_eval_ceiling_per_hour, 500_000);
        let parse = |s: &str| {
            s.parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
                .ok_or_else(|| anyhow!("a whole number of at least 0"))
        };
        assert_eq!(parse("0").unwrap(), 0);
        assert!(parse("-1").is_err() && parse("lots").is_err());
    }

    #[test]
    fn booleans_are_strict() {
        assert!(bool_value("TRUE").unwrap());
        assert!(!bool_value("0").unwrap());
        assert!(bool_value("yes-please").is_err());
    }

    #[test]
    fn the_retired_quota_switch_is_recognised_whatever_its_value() {
        // Presence is what counts: `FLAGS_ENFORCE_QUOTAS=true` boots (nothing
        // parses it) and is named for the startup warning.
        let set = retired_set(|name| name == "FLAGS_ENFORCE_QUOTAS");
        assert_eq!(set, vec!["FLAGS_ENFORCE_QUOTAS"]);
        assert!(retired_set(|_| false).is_empty());
        assert!(retired_warning("FLAGS_ENFORCE_QUOTAS").contains("FLAGS_ENFORCE_QUOTAS"));
    }

    /// The only test that touches the process environment.
    #[test]
    fn a_deployment_still_setting_the_retired_switch_boots() {
        for (k, v) in [
            ("FLAGS_DATABASE_URL", "postgres://localhost/test"),
            ("FLAGS_PUBLIC_URL", "https://flags.example.com"),
            ("FLAGS_PLATFORM_URL", "https://platform.example.com"),
            ("FLAGS_INTROSPECTION_SECRET", "otto_rs_test"),
            ("FLAGS_PLATFORM_WEBHOOK_SECRET", "otto_whsec_test"),
            ("FLAGS_ENFORCE_QUOTAS", "true"),
        ] {
            std::env::set_var(k, v);
        }
        let config = Config::from_env().expect("boots");
        assert_eq!(config.retired_set, vec!["FLAGS_ENFORCE_QUOTAS"]);
        std::env::remove_var("FLAGS_ENFORCE_QUOTAS");
        assert!(Config::from_env().expect("boots").retired_set.is_empty());
    }

    #[test]
    fn debug_prints_no_secret() {
        let s = format!("{:?}", Config::for_test());
        assert!(!s.contains("otto_rs_test") && !s.contains("otto_whsec_test"));
    }
}
