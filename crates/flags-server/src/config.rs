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
    /// Public origin, e.g. `https://otto-flags.savvagent.com`.
    pub public_url: String,
    /// Token audience; exactly the `resource_uri` registered at the platform.
    /// Defaults to `{public_url}/mcp`.
    pub resource_uri: String,
    pub platform_url: String,
    /// `otto_rs_…`: this service's credential at the platform.
    pub introspection_secret: String,
    /// `otto_whsec_…`: verifies the platform's lifecycle webhooks.
    pub platform_webhook_secret: String,
    pub extra_allowed_hosts: Vec<String>,
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
            extra_allowed_hosts: list("FLAGS_ALLOWED_HOSTS"),
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
    optional(name)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
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
