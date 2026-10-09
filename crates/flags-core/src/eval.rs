//! Flag evaluation: given a flag and a context, is it on, and which variation?
//!
//! Pure functions, no I/O. The SDK surface calls [`evaluate`] on the hot path,
//! the `evaluate_flag` MCP tool calls it for a dry run, and the write tools call
//! the `validate_*` functions so a configuration that cannot be evaluated never
//! reaches the database.
//!
//! ## The shape of a flag's state
//!
//! `feature_flags.environments` holds one [`EnvConfig`] per environment name:
//!
//! ```json
//! {"production": {
//!    "enabled": true,
//!    "rollout_percentage": 25,
//!    "rules": [{"attribute": "plan", "operator": "in", "values": ["enterprise"]}],
//!    "default_variation": "treatment"
//! }}
//! ```
//!
//! `feature_flags.variations` holds named [`Variation`]s, each with an optional
//! weight and configuration.
//!
//! ## Order of evaluation
//!
//! 1. Archived, no config for the environment, or `enabled: false` → **off**.
//! 2. Rules, in order. The first whose condition matches decides: on or off
//!    (`enabled`, default on), and optionally which variation.
//! 3. Otherwise the percentage rollout. The caller's identifier (`user_id`,
//!    else `anonymous_id`, else `session_id`) is hashed with the flag key into a
//!    bucket in `[0, 10000)`; the flag is on when the bucket is below
//!    `rollout_percentage × 100`. The same identifier always lands in the same
//!    bucket for a flag, so raising the percentage only ever adds people.
//!    Without an identifier a partial rollout is off (0 % and 100 % need none).
//! 4. When on and the flag has variations: the rule's variation, else
//!    `default_variation`, else a weighted pick bucketed independently of the
//!    rollout (so being in the rollout does not bias the variation).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Buckets per 100 %: two decimal places of rollout precision.
const BUCKETS: u64 = 10_000;
const MAX_RULES: usize = 50;
const MAX_RULE_VALUES: usize = 500;
const MAX_VARIATIONS: usize = 20;

/// One environment's state for a flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvConfig {
    /// Off means off for everyone, whatever the rules and rollout say.
    #[serde(default)]
    pub enabled: bool,
    /// Share of identified callers the flag is on for, 0–100, when no rule
    /// matched. Absent means 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_percentage: Option<f64>,
    /// Targeting rules, first match wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
    /// The variation served when the flag is on and no rule names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_variation: Option<String>,
}

/// A targeting rule: when `attribute` `operator` `values` holds, the flag is
/// `enabled` (default true) and serves `variation` (if given).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// A context field (`user_id`, `anonymous_id`, `session_id`, `language`,
    /// `organization_id`, `application_id`) or a key in `attributes`. Spell a
    /// custom attribute that shadows one of those as `attributes.<name>`.
    pub attribute: String,
    pub operator: Operator,
    /// Compared against the attribute. One value for scalar operators; any
    /// number for `in`/`not_in`; none for `exists`/`not_exists`.
    #[serde(default)]
    pub values: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variation: Option<String>,
    /// Free text for humans and agents reading the rule later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    In,
    NotIn,
    Equals,
    NotEquals,
    Contains,
    StartsWith,
    EndsWith,
    Gt,
    Gte,
    Lt,
    Lte,
    Exists,
    NotExists,
}

/// A named variation of a flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Variation {
    /// Relative weight in the split when neither a rule nor
    /// `default_variation` picks a variation. Absent means 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// Dynamic configuration served with this variation, replacing the flag's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The evaluation context an SDK sends. Field names are the SDKs' own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anonymous_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    #[schemars(with = "BTreeMap<String, Value>")]
    pub attributes: Map<String, Value>,
}

impl Context {
    fn identifier(&self) -> Option<&str> {
        [&self.user_id, &self.anonymous_id, &self.session_id]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .find(|s| !s.is_empty())
    }

    fn lookup(&self, attribute: &str) -> Option<Value> {
        if let Some(name) = attribute.strip_prefix("attributes.") {
            return self.attributes.get(name).cloned();
        }
        let builtin = match attribute {
            "user_id" => &self.user_id,
            "anonymous_id" => &self.anonymous_id,
            "session_id" => &self.session_id,
            "environment" => &self.environment,
            "language" => &self.language,
            "organization_id" => &self.organization_id,
            "application_id" => &self.application_id,
            _ => return self.attributes.get(attribute).cloned(),
        };
        builtin.clone().map(Value::String)
    }
}

/// Why an evaluation came out the way it did. Shown to agents in a dry run.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    Archived,
    /// The flag has no configuration for this environment.
    EnvironmentNotConfigured,
    Disabled,
    /// Rule number `index` (0-based) matched.
    Rule {
        index: usize,
    },
    /// The percentage rollout decided, with this caller's bucket (0–9999).
    Rollout {
        bucket: u64,
        threshold: u64,
    },
    /// A partial rollout and no identifier to bucket on.
    NoIdentifier,
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Evaluation {
    pub enabled: bool,
    pub variation: Option<String>,
    pub configuration: Option<Value>,
    pub reason: Reason,
}

/// What [`evaluate`] needs to know about a flag.
#[derive(Debug, Clone, Copy)]
pub struct FlagView<'a> {
    pub key: &'a str,
    pub archived: bool,
    pub environments: &'a Value,
    pub variations: &'a Value,
    pub configuration: Option<&'a Value>,
}

/// Evaluate a flag for a context in an environment.
///
/// Never fails: stored state was validated on the way in, and anything that
/// still does not parse evaluates as off rather than taking a request down.
pub fn evaluate(flag: FlagView<'_>, environment: &str, ctx: &Context) -> Evaluation {
    let off = |reason| Evaluation {
        enabled: false,
        variation: None,
        configuration: flag.configuration.cloned(),
        reason,
    };
    if flag.archived {
        return off(Reason::Archived);
    }
    let Some(env) = flag
        .environments
        .get(environment)
        .and_then(|v| serde_json::from_value::<EnvConfig>(v.clone()).ok())
    else {
        return off(Reason::EnvironmentNotConfigured);
    };
    if !env.enabled {
        return off(Reason::Disabled);
    }

    let variations = parse_variations(flag.variations).unwrap_or_default();

    for (index, rule) in env.rules.iter().enumerate() {
        if matches(rule, ctx) {
            if !rule.enabled.unwrap_or(true) {
                return off(Reason::Rule { index });
            }
            return on(
                flag,
                &variations,
                &env,
                rule.variation.as_deref(),
                ctx,
                Reason::Rule { index },
            );
        }
    }

    let pct = env.rollout_percentage.unwrap_or(100.0).clamp(0.0, 100.0);
    let threshold = (pct * 100.0).round() as u64;
    if threshold >= BUCKETS {
        let reason = Reason::Rollout {
            bucket: ctx
                .identifier()
                .map_or(0, |id| bucket("rollout", flag.key, id)),
            threshold,
        };
        return on(flag, &variations, &env, None, ctx, reason);
    }
    if threshold == 0 {
        return off(Reason::Rollout {
            bucket: ctx
                .identifier()
                .map_or(0, |id| bucket("rollout", flag.key, id)),
            threshold,
        });
    }
    let Some(id) = ctx.identifier() else {
        return off(Reason::NoIdentifier);
    };
    let b = bucket("rollout", flag.key, id);
    let reason = Reason::Rollout {
        bucket: b,
        threshold,
    };
    if b < threshold {
        on(flag, &variations, &env, None, ctx, reason)
    } else {
        off(reason)
    }
}

fn on(
    flag: FlagView<'_>,
    variations: &BTreeMap<String, Variation>,
    env: &EnvConfig,
    chosen: Option<&str>,
    ctx: &Context,
    reason: Reason,
) -> Evaluation {
    let name = chosen
        .or(env.default_variation.as_deref())
        .map(str::to_string)
        .or_else(|| pick_weighted(flag.key, variations, ctx));
    let configuration = name
        .as_deref()
        .and_then(|n| variations.get(n))
        .and_then(|v| v.configuration.clone())
        .or_else(|| flag.configuration.cloned());
    Evaluation {
        enabled: true,
        variation: name,
        configuration,
        reason,
    }
}

fn pick_weighted(
    flag_key: &str,
    variations: &BTreeMap<String, Variation>,
    ctx: &Context,
) -> Option<String> {
    if variations.is_empty() {
        return None;
    }
    let total: u64 = variations
        .values()
        .map(|v| u64::from(v.weight.unwrap_or(1)))
        .sum();
    if total == 0 {
        return variations.keys().next().cloned();
    }
    // Callers with no identifier all get the first variation: a stable answer
    // beats a random one that flickers on every request.
    let Some(id) = ctx.identifier() else {
        return variations.keys().next().cloned();
    };
    let point = bucket("variation", flag_key, id) * total / BUCKETS;
    let mut acc = 0;
    for (name, v) in variations {
        acc += u64::from(v.weight.unwrap_or(1));
        if point < acc {
            return Some(name.clone());
        }
    }
    variations.keys().last().cloned()
}

/// A stable bucket in `[0, BUCKETS)` for `id` on `flag_key`.
fn bucket(salt: &str, flag_key: &str, id: &str) -> u64 {
    let digest = Sha256::digest(format!("{salt}:{flag_key}:{id}").as_bytes());
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first) % BUCKETS
}

fn matches(rule: &Rule, ctx: &Context) -> bool {
    let actual = ctx.lookup(&rule.attribute);
    match rule.operator {
        Operator::Exists => return actual.is_some_and(|v| !v.is_null()),
        Operator::NotExists => return actual.is_none_or(|v| v.is_null()),
        _ => {}
    }
    let Some(actual) = actual.filter(|v| !v.is_null()) else {
        // A missing attribute satisfies only the negative operators.
        return matches!(rule.operator, Operator::NotIn | Operator::NotEquals);
    };
    let first = rule.values.first();
    match rule.operator {
        Operator::In => rule.values.iter().any(|v| loosely_equal(&actual, v)),
        Operator::NotIn => !rule.values.iter().any(|v| loosely_equal(&actual, v)),
        Operator::Equals => first.is_some_and(|v| loosely_equal(&actual, v)),
        Operator::NotEquals => !first.is_some_and(|v| loosely_equal(&actual, v)),
        Operator::Contains => match (&actual, first) {
            (Value::Array(items), Some(v)) => items.iter().any(|i| loosely_equal(i, v)),
            (_, Some(v)) => text(&actual)
                .zip(text(v))
                .is_some_and(|(a, b)| a.contains(&b)),
            _ => false,
        },
        Operator::StartsWith => text(&actual)
            .zip(first.and_then(text))
            .is_some_and(|(a, b)| a.starts_with(&b)),
        Operator::EndsWith => text(&actual)
            .zip(first.and_then(text))
            .is_some_and(|(a, b)| a.ends_with(&b)),
        Operator::Gt | Operator::Gte | Operator::Lt | Operator::Lte => {
            let (Some(a), Some(b)) = (number(&actual), first.and_then(number)) else {
                return false;
            };
            match rule.operator {
                Operator::Gt => a > b,
                Operator::Gte => a >= b,
                Operator::Lt => a < b,
                _ => a <= b,
            }
        }
        Operator::Exists | Operator::NotExists => unreachable!(),
    }
}

/// Equality that forgives the string/number/bool mismatches a context built in
/// a dynamically typed SDK produces: `"42"` equals `42`, `"true"` equals `true`.
fn loosely_equal(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    match (number(a), number(b)) {
        (Some(x), Some(y)) => return x == y,
        _ => {}
    }
    text(a).zip(text(b)).is_some_and(|(x, y)| x == y)
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse_variations(v: &Value) -> Option<BTreeMap<String, Variation>> {
    if v.is_null() {
        return Some(BTreeMap::new());
    }
    serde_json::from_value(v.clone()).ok()
}

// ------------------------------------------------------------------ validation

/// Validate a flag's variations, returning the canonical JSON to store.
pub fn validate_variations(v: &Value) -> Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Object(Map::new()));
    }
    let parsed: BTreeMap<String, Variation> = serde_json::from_value(v.clone()).map_err(|e| {
        format!(
            "variations must be an object of name -> {{\"weight\"?: integer, \
             \"configuration\"?: any, \"description\"?: string}}: {e}"
        )
    })?;
    if parsed.len() > MAX_VARIATIONS {
        return Err(format!(
            "a flag may have at most {MAX_VARIATIONS} variations"
        ));
    }
    for name in parsed.keys() {
        validate_name("variation name", name)?;
    }
    serde_json::to_value(&parsed).map_err(|e| e.to_string())
}

/// Validate one environment's configuration against the flag's variations,
/// returning it in canonical form.
pub fn validate_env(cfg: &EnvConfig, variations: &Value) -> Result<EnvConfig, String> {
    let names = parse_variations(variations).unwrap_or_default();
    let known = || {
        if names.is_empty() {
            "this flag has no variations; add some with update_flag first".to_string()
        } else {
            format!(
                "this flag's variations are: {}",
                names.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        }
    };
    if let Some(p) = cfg.rollout_percentage {
        if !(0.0..=100.0).contains(&p) || p.is_nan() {
            return Err(format!(
                "rollout_percentage must be between 0 and 100, not {p}"
            ));
        }
    }
    if let Some(v) = &cfg.default_variation {
        if !names.contains_key(v) {
            return Err(format!(
                "default_variation {v:?} is not a variation; {}",
                known()
            ));
        }
    }
    if cfg.rules.len() > MAX_RULES {
        return Err(format!("an environment may have at most {MAX_RULES} rules"));
    }
    for (i, rule) in cfg.rules.iter().enumerate() {
        let attr = rule.attribute.trim();
        if attr.is_empty() || attr.len() > 128 {
            return Err(format!("rule {i}: attribute must be 1-128 characters"));
        }
        if let Some(v) = &rule.variation {
            if !names.contains_key(v) {
                return Err(format!(
                    "rule {i}: variation {v:?} is not a variation; {}",
                    known()
                ));
            }
        }
        let n = rule.values.len();
        let ok = match rule.operator {
            Operator::Exists | Operator::NotExists => n == 0,
            Operator::In | Operator::NotIn => (1..=MAX_RULE_VALUES).contains(&n),
            _ => n == 1,
        };
        if !ok {
            return Err(format!(
                "rule {i}: operator {:?} takes {} value(s), got {n}",
                rule.operator,
                match rule.operator {
                    Operator::Exists | Operator::NotExists => "no".to_string(),
                    Operator::In | Operator::NotIn => format!("1 to {MAX_RULE_VALUES}"),
                    _ => "exactly one".to_string(),
                }
            ));
        }
        if matches!(
            rule.operator,
            Operator::Gt | Operator::Gte | Operator::Lt | Operator::Lte
        ) && number(&rule.values[0]).is_none()
        {
            return Err(format!(
                "rule {i}: {:?} compares numbers; give a number",
                rule.operator
            ));
        }
    }
    Ok(cfg.clone())
}

/// Check that every environment of a flag is still valid against a (possibly
/// new) set of variations. Used when variations change, so a removed variation
/// that a rule still names is caught at the write, not at evaluation.
pub fn validate_all_envs(environments: &Value, variations: &Value) -> Result<(), String> {
    let Some(map) = environments.as_object() else {
        return Ok(());
    };
    for (env, cfg) in map {
        let parsed: EnvConfig =
            serde_json::from_value(cfg.clone()).map_err(|e| format!("environment {env:?}: {e}"))?;
        validate_env(&parsed, variations).map_err(|e| format!("environment {env:?}: {e}"))?;
    }
    Ok(())
}

/// Names of apps, environments, and variations: short, and safe in a URL.
pub fn validate_name(what: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!("{what} must be 1-64 characters, got {name:?}"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "{what} {name:?} may contain only letters, digits, '-', '_' and '.'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view<'a>(envs: &'a Value, vars: &'a Value) -> FlagView<'a> {
        FlagView {
            key: "new-checkout",
            archived: false,
            environments: envs,
            variations: vars,
            configuration: None,
        }
    }

    fn user(id: &str) -> Context {
        Context {
            user_id: Some(id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn missing_disabled_and_archived_flags_are_off() {
        let none = json!({});
        let vars = json!({});
        assert_eq!(
            evaluate(view(&none, &vars), "production", &user("u")).reason,
            Reason::EnvironmentNotConfigured
        );
        let disabled = json!({"production": {"enabled": false}});
        let e = evaluate(view(&disabled, &vars), "production", &user("u"));
        assert!(!e.enabled);
        assert_eq!(e.reason, Reason::Disabled);
        let on = json!({"production": {"enabled": true}});
        let mut f = view(&on, &vars);
        f.archived = true;
        assert_eq!(
            evaluate(f, "production", &user("u")).reason,
            Reason::Archived
        );
    }

    #[test]
    fn a_full_rollout_needs_no_identifier_and_a_partial_one_does() {
        let vars = json!({});
        let full = json!({"production": {"enabled": true}});
        assert!(evaluate(view(&full, &vars), "production", &Context::default()).enabled);
        let half = json!({"production": {"enabled": true, "rollout_percentage": 50}});
        let e = evaluate(view(&half, &vars), "production", &Context::default());
        assert!(!e.enabled);
        assert_eq!(e.reason, Reason::NoIdentifier);
    }

    #[test]
    fn rollout_is_stable_per_user_and_close_to_the_percentage() {
        let vars = json!({});
        let envs = json!({"production": {"enabled": true, "rollout_percentage": 25}});
        let on = (0..10_000)
            .filter(|i| evaluate(view(&envs, &vars), "production", &user(&format!("u{i}"))).enabled)
            .count();
        assert!(
            (2_200..2_800).contains(&on),
            "25% rollout put {on} of 10000 users in"
        );
        let first = evaluate(view(&envs, &vars), "production", &user("u7"));
        for _ in 0..5 {
            assert_eq!(
                evaluate(view(&envs, &vars), "production", &user("u7")),
                first
            );
        }
    }

    /// Raising the percentage must only ever add people. Anyone in at 10 % is
    /// still in at 50 %.
    #[test]
    fn raising_the_rollout_never_removes_anyone() {
        let vars = json!({});
        let low = json!({"production": {"enabled": true, "rollout_percentage": 10}});
        let high = json!({"production": {"enabled": true, "rollout_percentage": 50}});
        for i in 0..2_000 {
            let u = user(&format!("u{i}"));
            if evaluate(view(&low, &vars), "production", &u).enabled {
                assert!(evaluate(view(&high, &vars), "production", &u).enabled);
            }
        }
    }

    #[test]
    fn the_first_matching_rule_wins_and_can_turn_the_flag_off() {
        let vars = json!({"control": {}, "treatment": {"configuration": {"color": "blue"}}});
        let envs = json!({"production": {
            "enabled": true,
            "rollout_percentage": 0,
            "rules": [
                {"attribute": "country", "operator": "in", "values": ["CU", "IR"], "enabled": false},
                {"attribute": "plan", "operator": "equals", "values": ["enterprise"], "variation": "treatment"}
            ]
        }});
        let mut ctx = user("u1");
        ctx.attributes.insert("plan".into(), json!("enterprise"));
        let e = evaluate(view(&envs, &vars), "production", &ctx);
        assert!(e.enabled);
        assert_eq!(e.variation.as_deref(), Some("treatment"));
        assert_eq!(e.configuration, Some(json!({"color": "blue"})));
        assert_eq!(e.reason, Reason::Rule { index: 1 });

        ctx.attributes.insert("country".into(), json!("IR"));
        let e = evaluate(view(&envs, &vars), "production", &ctx);
        assert!(!e.enabled);
        assert_eq!(e.reason, Reason::Rule { index: 0 });

        // No rule matches and the rollout is 0 %: off.
        assert!(!evaluate(view(&envs, &vars), "production", &user("u2")).enabled);
    }

    #[test]
    fn operators_compare_loosely_typed_context_values() {
        let mut ctx = user("u1");
        ctx.attributes.insert("age".into(), json!("42"));
        ctx.attributes
            .insert("email".into(), json!("ada@example.com"));
        ctx.attributes
            .insert("tags".into(), json!(["beta", "staff"]));
        let rule = |attribute: &str, operator, values: Value| Rule {
            attribute: attribute.into(),
            operator,
            values: values.as_array().cloned().unwrap_or_default(),
            enabled: None,
            variation: None,
            description: None,
        };
        assert!(matches(&rule("age", Operator::Gte, json!([42])), &ctx));
        assert!(!matches(&rule("age", Operator::Lt, json!([40])), &ctx));
        assert!(matches(&rule("age", Operator::Equals, json!([42])), &ctx));
        assert!(matches(
            &rule("email", Operator::EndsWith, json!(["@example.com"])),
            &ctx
        ));
        assert!(matches(
            &rule("tags", Operator::Contains, json!(["beta"])),
            &ctx
        ));
        assert!(matches(
            &rule("user_id", Operator::In, json!(["u0", "u1"])),
            &ctx
        ));
        assert!(matches(
            &rule("missing", Operator::NotExists, json!([])),
            &ctx
        ));
        assert!(matches(
            &rule("missing", Operator::NotIn, json!(["x"])),
            &ctx
        ));
        assert!(!matches(&rule("missing", Operator::In, json!(["x"])), &ctx));
        assert!(matches(
            &rule("attributes.email", Operator::Exists, json!([])),
            &ctx
        ));
    }

    #[test]
    fn weighted_variations_split_close_to_their_weights() {
        let vars = json!({"a": {"weight": 1}, "b": {"weight": 3}});
        let envs = json!({"production": {"enabled": true}});
        let b = (0..8_000)
            .filter(|i| {
                evaluate(view(&envs, &vars), "production", &user(&format!("u{i}")))
                    .variation
                    .as_deref()
                    == Some("b")
            })
            .count();
        assert!((5_600..6_400).contains(&b), "75% weight got {b} of 8000");
    }

    #[test]
    fn default_variation_overrides_the_split() {
        let vars = json!({"a": {}, "b": {}});
        let envs = json!({"production": {"enabled": true, "default_variation": "b"}});
        for i in 0..50 {
            let e = evaluate(view(&envs, &vars), "production", &user(&format!("u{i}")));
            assert_eq!(e.variation.as_deref(), Some("b"));
        }
    }

    #[test]
    fn validation_catches_what_evaluation_would_silently_ignore() {
        let vars = json!({"control": {}});
        let bad = |v: Value| -> String {
            let cfg: EnvConfig = serde_json::from_value(v).unwrap();
            validate_env(&cfg, &vars).unwrap_err()
        };
        assert!(
            bad(json!({"enabled": true, "rollout_percentage": 150})).contains("between 0 and 100")
        );
        assert!(bad(json!({"enabled": true, "default_variation": "nope"})).contains("control"));
        assert!(bad(json!({"enabled": true, "rules": [
            {"attribute": "plan", "operator": "equals", "values": []}
        ]}))
        .contains("exactly one"));
        assert!(bad(json!({"enabled": true, "rules": [
            {"attribute": "age", "operator": "gt", "values": ["old"]}
        ]}))
        .contains("number"));
        assert!(
            serde_json::from_value::<EnvConfig>(json!({"enabled": true, "rollout": 5})).is_err()
        );

        assert!(validate_variations(&json!({"bad name": {}})).is_err());
        assert!(validate_variations(&json!({"on": {"weight": -1}})).is_err());
        assert_eq!(validate_variations(&Value::Null).unwrap(), json!({}));

        let envs = json!({"production": {"enabled": true, "default_variation": "control"}});
        assert!(validate_all_envs(&envs, &json!({"treatment": {}})).is_err());
        assert!(validate_all_envs(&envs, &vars).is_ok());
    }
}
