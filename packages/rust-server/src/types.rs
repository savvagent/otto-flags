use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Configuration for the FlagClient
#[derive(Clone)]
pub struct Config {
    /// SDK API key (starts with sdk_)
    pub api_key: String,

    /// Application ID for application-scoped flags
    pub application_id: Option<String>,

    /// Base URL for the Otto Flags API
    pub base_url: String,

    /// Enable real-time flag updates via SSE
    pub enable_realtime: bool,

    /// Cache TTL duration
    pub cache_ttl: Duration,

    /// Report evaluations and errors to Otto Flags (see `FlagClient::track_error`)
    pub enable_telemetry: bool,

    /// Default flag values when evaluation fails
    pub defaults: HashMap<String, bool>,

    /// HTTP request timeout
    pub timeout: Duration,

    /// How long lookups return their default without a request after a failed one. A
    /// `Retry-After` on a 429 or 503 can lengthen the window (up to 60 s). A 404 pauses only
    /// that flag; a failure caused by the request itself (another 4xx except 401, 403, 408 and
    /// 429, or a response over 1 MiB) pauses nothing; any other failure pauses every flag.
    /// `Duration::ZERO` turns the cooldown off. A few seconds is sensible.
    pub failure_cooldown: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            application_id: None,
            base_url: "https://otto-flags.savvagent.com".to_string(),
            enable_realtime: true,
            cache_ttl: Duration::from_secs(60),
            enable_telemetry: true,
            defaults: HashMap::new(),
            timeout: Duration::from_secs(5),
            failure_cooldown: Duration::from_secs(5),
        }
    }
}

impl Config {
    /// Create a new Config with the given API key
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            ..Default::default()
        }
    }

    /// Set the application ID
    pub fn with_application_id(mut self, app_id: impl Into<String>) -> Self {
        self.application_id = Some(app_id.into());
        self
    }

    /// Set the base URL
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Set cache TTL
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// Set request timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set how long lookups return their default without a request after a failed one (5 s
    /// by default); `Duration::ZERO` turns the cooldown off
    pub fn with_failure_cooldown(mut self, cooldown: Duration) -> Self {
        self.failure_cooldown = cooldown;
        self
    }

    /// Turn telemetry (evaluation counts and reported errors) on or off; on by default
    pub fn with_telemetry(mut self, enabled: bool) -> Self {
        self.enable_telemetry = enabled;
        self
    }

    /// Add a default flag value
    pub fn with_default(mut self, key: impl Into<String>, value: bool) -> Self {
        self.defaults.insert(key.into(), value);
        self
    }
}

/// Context for flag evaluation
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    /// User ID for targeted rollouts
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    /// Session ID for session-based rollouts
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Application ID for application-scoped flags
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,

    /// Custom attributes for targeting rules
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<HashMap<String, serde_json::Value>>,

    /// Environment (dev, staging, production)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,

    /// IP address for geo-targeting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,

    /// User agent string
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

impl Context {
    /// Create a new empty context
    pub fn new() -> Self {
        Self::default()
    }

    /// Set user ID
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Set session ID
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set environment
    pub fn with_environment(mut self, env: impl Into<String>) -> Self {
        self.environment = Some(env.into());
        self
    }

    /// Add an attribute
    pub fn with_attribute(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.attributes
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value);
        self
    }
}

/// Result from flag evaluation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    /// Flag key
    pub key: String,

    /// Evaluated value
    pub value: bool,

    /// Dynamic configuration attached to the flag (Phase 1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<serde_json::Value>,

    /// Variation identifier for multi-variant flags (Phase 2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variation: Option<String>,

    /// Reason for the value
    pub reason: String,

    /// Metadata about the flag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// Result from variation evaluation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariationResult {
    /// Variation identifier (e.g., "control", "variant_a")
    pub variation: String,

    /// Whether the flag is enabled
    pub enabled: bool,

    /// Dynamic configuration for this variation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<serde_json::Value>,
}

/// Metadata about a flag, from the evaluate response
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metadata {
    /// Not sent by the server; always `None`. Kept for compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_id: Option<String>,

    /// Not sent by the server; always `None`. Kept for compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Not sent by the server; always `None` (the variation is `EvaluationResult::variation`).
    /// Kept for compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,

    /// The flag version that produced this result
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,

    /// Where the flag is defined: `"application"` or `"enterprise"`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// An error the host raised while a flag was in play, reported with
/// `FlagClient::track_error`. Text is truncated to the server's limits (type 200, message
/// 2 000, stack trace 8 000 characters).
///
/// The message and stack trace are stored by Otto Flags (14 days) and shown to the agents that
/// manage your flags: keep end-user data (emails, ids, query values) out of them.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ErrorReport {
    /// The flag in play when the error happened
    pub flag_key: String,
    /// Whether the flag was on for this request
    pub flag_enabled: bool,
    /// The error's type, e.g. `"TimeoutError"`
    pub error_type: String,
    /// The error's message
    pub error_message: String,
    /// A stack trace or backtrace, if the host has one
    pub stack_trace: Option<String>,
    /// The environment, e.g. `"production"` (the server assumes production when absent)
    pub environment: Option<String>,
}

impl ErrorReport {
    /// A report with a type and message; add a stack trace or environment with the `with_`
    /// methods.
    pub fn new(
        flag_key: impl Into<String>,
        flag_enabled: bool,
        error_type: impl Into<String>,
        error_message: impl Into<String>,
    ) -> Self {
        Self {
            flag_key: flag_key.into(),
            flag_enabled,
            error_type: error_type.into(),
            error_message: error_message.into(),
            stack_trace: None,
            environment: None,
        }
    }

    /// A report for a Rust error: the last segment of its type name (e.g. `ParseIntError`) and
    /// its `Display` text, which may carry end-user data (see the type docs). A type-erased
    /// error (`dyn Error`, `Box<dyn Error>`, `anyhow::Error`) has no useful type name here; use
    /// `ErrorReport::new` with a type of your choosing for those.
    pub fn from_error<E: std::error::Error + ?Sized>(
        flag_key: impl Into<String>,
        flag_enabled: bool,
        error: &E,
    ) -> Self {
        let full = std::any::type_name::<E>();
        // Drop the module path (not stable across compiler versions) but keep generics whole.
        let base = full.split('<').next().unwrap_or(full);
        let start = base.rfind("::").map_or(0, |i| i + 2);
        Self::new(flag_key, flag_enabled, &full[start..], error.to_string())
    }

    /// Attach a stack trace or backtrace.
    pub fn with_stack_trace(mut self, stack_trace: impl Into<String>) -> Self {
        self.stack_trace = Some(stack_trace.into());
        self
    }

    /// Attach the environment, e.g. `"production"`.
    pub fn with_environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }
}

/// Options for setting configuration overrides
#[derive(Debug, Clone)]
pub struct ConfigOverrideOptions {
    /// Merge with API configuration instead of replacing (default: false)
    pub merge: bool,

    /// Validate configuration structure (default: true)
    pub validate: bool,
}

// Not derived: a derived `Default` would set `validate: false`, contradicting `new()` and the
// field docs.
impl Default for ConfigOverrideOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigOverrideOptions {
    pub fn new() -> Self {
        Self {
            merge: false,
            validate: true,
        }
    }

    pub fn with_merge(mut self, merge: bool) -> Self {
        self.merge = merge;
        self
    }

    pub fn with_validate(mut self, validate: bool) -> Self {
        self.validate = validate;
        self
    }
}

/// Internal structure for storing configuration overrides
#[derive(Debug, Clone)]
pub(crate) struct ConfigOverrideEntry {
    pub config: serde_json::Value,
    pub merge: bool,
    pub timestamp: std::time::SystemTime,
}

/// Internal structure for storing variation overrides
#[derive(Debug, Clone)]
pub(crate) struct VariationOverrideEntry {
    pub variation: String,
    pub timestamp: std::time::SystemTime,
}

#[cfg(test)]
mod tests {
    use super::ConfigOverrideOptions;

    // `set_config_override(.., None)` uses `unwrap_or_default()`, so `default()` must keep
    // validation on, matching `new()` and the field docs (#15).
    #[test]
    fn config_override_options_default_validates() {
        let opts = ConfigOverrideOptions::default();
        assert!(opts.validate);
        assert!(!opts.merge);
        // Catches a field added to `new()` but not to `Default`.
        assert_eq!(
            format!("{opts:?}"),
            format!("{:?}", ConfigOverrideOptions::new())
        );
    }
}
