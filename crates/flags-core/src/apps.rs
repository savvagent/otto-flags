//! Flag apps: what a running application authenticates as, and the scope every
//! flag lives in.
//!
//! An app has a name unique in its org (what agents call it by), a list of
//! environment names, and two SDK keys (see [`crate::keys`]). Creating an app
//! or rotating its keys returns the server key in clear exactly once.

use otto_tenant::audit::Entry;
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::Tx;
use serde::Serialize;
use sqlx::FromRow;

use crate::audit::action;
use crate::error::{on_unique, Error, Result};
use crate::eval::validate_name;
use crate::ids::FlagAppId;
use crate::keys::{KeyKind, NewKey};

const MAX_ENVIRONMENTS: usize = 20;
/// The most origins one app may allow (also a CHECK in migration 0011).
pub const MAX_ALLOWED_ORIGINS: usize = 20;
const MAX_ORIGIN_LEN: usize = 253;

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagApp {
    pub id: FlagAppId,
    pub org_id: OrgId,
    pub name: String,
    pub environments: Vec<String>,
    /// Browser origins the public client key may be used from. Empty means
    /// unrestricted. A deterrent against use from other sites, not
    /// authentication: see [`origin_allowed`].
    pub allowed_origins: Vec<String>,
    /// The public client-side key (`sdk_…`). Safe to embed in a web page.
    pub client_key: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Keys minted by a create or a rotation, in clear. Shown once.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IssuedKeys {
    /// New client key (`sdk_…`), if one was issued.
    pub client_key: Option<String>,
    /// New server key (`srv_…`), if one was issued. It is not stored in clear
    /// and cannot be shown again: put it in the app's secret store now.
    pub server_key: Option<String>,
}

/// Which keys a rotation replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Rotate {
    Client,
    Server,
    Both,
}

const COLS: &str = "id, org_id, name, environments, allowed_origins, client_key, created_at";

/// Normalize and validate an environment list: trimmed, deduplicated, order
/// kept, at least one.
pub fn normalize_environments(envs: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for e in envs {
        let e = e.trim().to_string();
        validate_name("environment name", &e).map_err(Error::Invalid)?;
        if !out.contains(&e) {
            out.push(e);
        }
    }
    if out.is_empty() {
        out.push("production".into());
    }
    if out.len() > MAX_ENVIRONMENTS {
        return Err(Error::Invalid(format!(
            "an app may have at most {MAX_ENVIRONMENTS} environments"
        )));
    }
    Ok(out)
}

/// Normalize and validate an origin allowlist: each entry `http(s)://host[:port]`
/// (lowercased, no path, query, userinfo or trailing slash), or
/// `http(s)://*.host[:port]` for every subdomain of `host` (not `host` itself).
/// Trimmed, deduplicated, order kept. Empty is valid and means unrestricted.
pub fn normalize_origins(origins: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for o in origins {
        let o = o.trim().to_ascii_lowercase();
        parse_origin(&o, true).ok_or_else(|| {
            Error::Invalid(format!(
                "allowed origin {o:?} is not an origin: use https://host[:port] (or \
                 https://*.host[:port] for subdomains), with no path, query or trailing slash"
            ))
        })?;
        if !out.contains(&o) {
            out.push(o);
        }
    }
    if out.len() > MAX_ALLOWED_ORIGINS {
        return Err(Error::Invalid(format!(
            "an app may allow at most {MAX_ALLOWED_ORIGINS} origins"
        )));
    }
    Ok(out)
}

/// `(scheme, host, port)` of a lowercase origin, or `None` if it is not
/// `http(s)://host[:port]`. With `pattern`, a leading `*.` label is accepted
/// and kept in the host.
fn parse_origin(o: &str, pattern: bool) -> Option<(&str, &str, Option<&str>)> {
    if o.len() > MAX_ORIGIN_LEN {
        return None;
    }
    let (scheme, rest) = o.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (rest, None),
    };
    if let Some(p) = port {
        let ok = !p.is_empty()
            && p.len() <= 5
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n));
        if !ok {
            return None;
        }
    }
    let name = match host.strip_prefix("*.") {
        Some(rest) if pattern => rest,
        _ => host,
    };
    let labels_ok = !name.is_empty()
        && name.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    labels_ok.then_some((scheme, host, port))
}

/// Whether a request's `Origin` header value is allowed by `allowed`.
///
/// An empty list allows everything. Otherwise the origin must equal an entry
/// (case-insensitive), or match a `https://*.host` entry: same scheme and
/// port, and a host with at least one more label in front of `host`. The
/// literal origin `null` (sandboxed frames, `file://`) matches nothing.
///
/// This is a deterrent against casual use of a public key from another site.
/// Only browsers set `Origin` honestly; a script can send any value or none,
/// so it is not authentication.
pub fn origin_allowed(allowed: &[String], origin: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let origin = origin.trim().to_ascii_lowercase();
    let Some((scheme, host, port)) = parse_origin(&origin, false) else {
        return false;
    };
    allowed.iter().any(|entry| {
        if *entry == origin {
            return true;
        }
        let Some((es, eh, ep)) = parse_origin(entry, true) else {
            return false;
        };
        match eh.strip_prefix("*.") {
            Some(suffix) => {
                es == scheme
                    && ep == port
                    && host.len() > suffix.len() + 1
                    && host.ends_with(suffix)
                    && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
            }
            None => false,
        }
    })
}

async fn insert_key(tx: &mut Tx<'_>, app: FlagAppId, key: &NewKey) -> Result<()> {
    let org = tx.org();
    sqlx::query(
        "INSERT INTO app_keys (key_hash, org_id, app_id, kind, prefix) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(key.hash())
    .bind(org)
    .bind(app)
    .bind(key.kind.as_str())
    .bind(key.shown_prefix())
    .execute(tx.conn())
    .await?;
    Ok(())
}

pub trait AppsExt {
    fn create_app(
        &mut self,
        name: &str,
        environments: &[String],
        actor: UserId,
    ) -> impl std::future::Future<Output = Result<(FlagApp, IssuedKeys)>> + Send;

    fn list_apps(&mut self) -> impl std::future::Future<Output = Result<Vec<FlagApp>>> + Send;

    fn get_app(
        &mut self,
        id: FlagAppId,
    ) -> impl std::future::Future<Output = Result<Option<FlagApp>>> + Send;

    /// Find an app by name (or by id, spelled as a uuid), or fail with an error
    /// listing the apps that do exist.
    fn resolve_app(
        &mut self,
        name_or_id: &str,
    ) -> impl std::future::Future<Output = Result<FlagApp>> + Send;

    fn set_app_environments(
        &mut self,
        app: &FlagApp,
        environments: &[String],
        actor: UserId,
    ) -> impl std::future::Future<Output = Result<FlagApp>> + Send;

    /// Replace the origin allowlist. Empty clears it (unrestricted). Setting
    /// the list it already has changes nothing and writes no audit entry.
    fn set_app_allowed_origins(
        &mut self,
        app: &FlagApp,
        origins: &[String],
        actor: UserId,
    ) -> impl std::future::Future<Output = Result<FlagApp>> + Send;

    fn rotate_keys(
        &mut self,
        app: &FlagApp,
        which: Rotate,
        actor: UserId,
    ) -> impl std::future::Future<Output = Result<(FlagApp, IssuedKeys)>> + Send;
}

impl AppsExt for Tx<'_> {
    async fn create_app(
        &mut self,
        name: &str,
        environments: &[String],
        actor: UserId,
    ) -> Result<(FlagApp, IssuedKeys)> {
        let name = name.trim();
        validate_name("app name", name).map_err(Error::Invalid)?;
        let environments = normalize_environments(environments)?;
        let client = NewKey::mint(KeyKind::Client);
        let server = NewKey::mint(KeyKind::Server);
        let org = self.org();

        let app: FlagApp = sqlx::query_as(&format!(
            "INSERT INTO flag_apps (org_id, name, environments, client_key, created_by) \
             VALUES ($1,$2,$3,$4,$5) RETURNING {COLS}"
        ))
        .bind(org)
        .bind(name)
        .bind(&environments)
        .bind(&client.key)
        .bind(actor)
        .fetch_one(self.conn())
        .await
        .map_err(|e| {
            on_unique(e, || Error::DuplicateAppName {
                name: name.to_string(),
            })
        })?;
        insert_key(self, app.id, &client).await?;
        insert_key(self, app.id, &server).await?;

        self.audit(
            Entry::new(action::APP_CREATED)
                .actor(actor)
                .target("flag_app", app.id.to_string())
                .detail(serde_json::json!({"name": app.name, "environments": app.environments})),
        )
        .await?;

        Ok((
            app,
            IssuedKeys {
                client_key: Some(client.key),
                server_key: Some(server.key),
            },
        ))
    }

    async fn list_apps(&mut self) -> Result<Vec<FlagApp>> {
        let org = self.org();
        Ok(sqlx::query_as(&format!(
            "SELECT {COLS} FROM flag_apps WHERE org_id = $1 ORDER BY name"
        ))
        .bind(org)
        .fetch_all(self.conn())
        .await?)
    }

    async fn get_app(&mut self, id: FlagAppId) -> Result<Option<FlagApp>> {
        let org = self.org();
        Ok(sqlx::query_as(&format!(
            "SELECT {COLS} FROM flag_apps WHERE org_id = $1 AND id = $2"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(self.conn())
        .await?)
    }

    async fn resolve_app(&mut self, name_or_id: &str) -> Result<FlagApp> {
        let wanted = name_or_id.trim();
        let org = self.org();
        let id = uuid::Uuid::parse_str(wanted).ok();
        let found: Option<FlagApp> = if crate::eval::unstorable(wanted) {
            None
        } else {
            sqlx::query_as(&format!(
                "SELECT {COLS} FROM flag_apps WHERE org_id = $1 AND (name = $2 OR id = $3)"
            ))
            .bind(org)
            .bind(wanted)
            .bind(id)
            .fetch_optional(self.conn())
            .await?
        };
        if let Some(app) = found {
            return Ok(app);
        }
        let names: Vec<String> = self
            .list_apps()
            .await?
            .into_iter()
            .map(|a| a.name)
            .collect();
        Err(Error::AppNotFound {
            name: wanted.to_string(),
            hint: if names.is_empty() {
                "it has no apps yet; create one with create_app".into()
            } else {
                format!("its apps are: {}", names.join(", "))
            },
        })
    }

    async fn set_app_environments(
        &mut self,
        app: &FlagApp,
        environments: &[String],
        actor: UserId,
    ) -> Result<FlagApp> {
        let environments = normalize_environments(environments)?;
        let org = self.org();
        let updated: FlagApp = sqlx::query_as(&format!(
            "UPDATE flag_apps SET environments = $3 WHERE org_id = $1 AND id = $2 RETURNING {COLS}"
        ))
        .bind(org)
        .bind(app.id)
        .bind(&environments)
        .fetch_one(self.conn())
        .await?;
        self.audit(
            Entry::new(action::APP_UPDATED)
                .actor(actor)
                .target("flag_app", app.id.to_string())
                .detail(serde_json::json!({
                    "environments": {"from": app.environments, "to": updated.environments}
                })),
        )
        .await?;
        Ok(updated)
    }

    async fn set_app_allowed_origins(
        &mut self,
        app: &FlagApp,
        origins: &[String],
        actor: UserId,
    ) -> Result<FlagApp> {
        let origins = normalize_origins(origins)?;
        if origins == app.allowed_origins {
            return Ok(app.clone());
        }
        let org = self.org();
        let updated: FlagApp = sqlx::query_as(&format!(
            "UPDATE flag_apps SET allowed_origins = $3 WHERE org_id = $1 AND id = $2 \
             RETURNING {COLS}"
        ))
        .bind(org)
        .bind(app.id)
        .bind(&origins)
        .fetch_one(self.conn())
        .await?;
        self.audit(
            Entry::new(action::APP_UPDATED)
                .actor(actor)
                .target("flag_app", app.id.to_string())
                .detail(serde_json::json!({
                    "allowed_origins": {"from": app.allowed_origins, "to": updated.allowed_origins}
                })),
        )
        .await?;
        Ok(updated)
    }

    async fn rotate_keys(
        &mut self,
        app: &FlagApp,
        which: Rotate,
        actor: UserId,
    ) -> Result<(FlagApp, IssuedKeys)> {
        let org = self.org();
        let kinds: &[KeyKind] = match which {
            Rotate::Client => &[KeyKind::Client],
            Rotate::Server => &[KeyKind::Server],
            Rotate::Both => &[KeyKind::Client, KeyKind::Server],
        };
        let mut issued = IssuedKeys {
            client_key: None,
            server_key: None,
        };
        let mut current = app.clone();
        for kind in kinds {
            // The old key stops working the moment this commits.
            sqlx::query("DELETE FROM app_keys WHERE org_id = $1 AND app_id = $2 AND kind = $3")
                .bind(org)
                .bind(app.id)
                .bind(kind.as_str())
                .execute(self.conn())
                .await?;
            let key = NewKey::mint(*kind);
            insert_key(self, app.id, &key).await?;
            match kind {
                KeyKind::Client => {
                    current = sqlx::query_as(&format!(
                        "UPDATE flag_apps SET client_key = $3 WHERE org_id = $1 AND id = $2 \
                         RETURNING {COLS}"
                    ))
                    .bind(org)
                    .bind(app.id)
                    .bind(&key.key)
                    .fetch_one(self.conn())
                    .await?;
                    issued.client_key = Some(key.key);
                }
                KeyKind::Server => issued.server_key = Some(key.key),
            }
        }
        self.audit(
            Entry::new(action::APP_KEYS_ROTATED)
                .actor(actor)
                .target("flag_app", app.id.to_string())
                .detail(serde_json::json!({
                    "kinds": kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>()
                })),
        )
        .await?;
        Ok((current, issued))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn origins_are_trimmed_lowercased_and_deduplicated() {
        let got = normalize_origins(&v(&[
            " HTTPS://App.Example.com ",
            "https://app.example.com",
            "http://localhost:3000",
            "https://*.example.org",
        ]))
        .unwrap();
        assert_eq!(
            got,
            v(&[
                "https://app.example.com",
                "http://localhost:3000",
                "https://*.example.org"
            ])
        );
        assert!(normalize_origins(&[]).unwrap().is_empty());
    }

    #[test]
    fn malformed_origins_are_refused() {
        for bad in [
            "",
            "example.com",
            "ftp://example.com",
            "https://",
            "https://example.com/",
            "https://example.com/path",
            "https://example.com?x=1",
            "https://user@example.com",
            "https://exa mple.com",
            "https://example.com:",
            "https://example.com:0",
            "https://example.com:99999",
            "https://example.com:80a",
            "https://*",
            "https://*.",
            "https://a.*.example.com",
            "https://*example.com",
            "https://-bad.example.com",
            "https://exa_mple.com",
            "null",
            "*",
        ] {
            assert!(
                normalize_origins(&v(&[bad])).is_err(),
                "{bad:?} should be refused"
            );
        }
        let long = format!("https://{}.com", "a".repeat(260));
        assert!(normalize_origins(&[long]).is_err());
    }

    #[test]
    fn at_most_twenty_origins() {
        let ok: Vec<String> = (0..MAX_ALLOWED_ORIGINS)
            .map(|i| format!("https://a{i}.example.com"))
            .collect();
        assert_eq!(normalize_origins(&ok).unwrap().len(), MAX_ALLOWED_ORIGINS);
        let mut too_many = ok;
        too_many.push("https://over.example.com".into());
        assert!(normalize_origins(&too_many).is_err());
    }

    #[test]
    fn an_empty_list_allows_everything() {
        assert!(origin_allowed(&[], "https://anywhere.example"));
        assert!(origin_allowed(&[], "null"));
    }

    #[test]
    fn an_exact_origin_matches_scheme_host_and_port() {
        let list = v(&["https://app.example.com", "http://localhost:3000"]);
        assert!(origin_allowed(&list, "https://app.example.com"));
        assert!(origin_allowed(&list, "HTTPS://APP.example.com"));
        assert!(origin_allowed(&list, "http://localhost:3000"));
        assert!(!origin_allowed(&list, "http://app.example.com"));
        assert!(!origin_allowed(&list, "https://app.example.com:8443"));
        assert!(!origin_allowed(&list, "http://localhost:3001"));
        assert!(!origin_allowed(&list, "https://evil.example.com"));
        assert!(!origin_allowed(&list, "https://app.example.com.evil.com"));
        assert!(!origin_allowed(&list, "null"));
        assert!(!origin_allowed(&list, "garbage"));
    }

    #[test]
    fn a_wildcard_matches_subdomains_only() {
        let list = v(&["https://*.example.com"]);
        assert!(origin_allowed(&list, "https://app.example.com"));
        assert!(origin_allowed(&list, "https://a.b.example.com"));
        assert!(!origin_allowed(&list, "https://example.com"));
        assert!(!origin_allowed(&list, "https://evilexample.com"));
        assert!(!origin_allowed(&list, "https://app.example.com.evil.com"));
        assert!(!origin_allowed(&list, "http://app.example.com"));
        assert!(!origin_allowed(&list, "https://app.example.com:8443"));
        let with_port = v(&["https://*.example.com:8443"]);
        assert!(origin_allowed(&with_port, "https://app.example.com:8443"));
        assert!(!origin_allowed(&with_port, "https://app.example.com"));
    }
}
