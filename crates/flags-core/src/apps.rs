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

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagApp {
    pub id: FlagAppId,
    pub org_id: OrgId,
    pub name: String,
    pub environments: Vec<String>,
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

const COLS: &str = "id, org_id, name, environments, client_key, created_at";

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
        let found: Option<FlagApp> = sqlx::query_as(&format!(
            "SELECT {COLS} FROM flag_apps WHERE org_id = $1 AND (name = $2 OR id = $3)"
        ))
        .bind(org)
        .bind(wanted)
        .bind(id)
        .fetch_optional(self.conn())
        .await?;
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
