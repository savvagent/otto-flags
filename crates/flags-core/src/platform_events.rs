//! What the platform's signed lifecycle webhooks do to this database.
//!
//! The platform owns orgs and memberships; this database holds rows keyed by
//! their ids with no foreign key to them. When the platform deletes one it
//! tells every registered resource server, and this module is otto-flags'
//! cleanup. Deliveries are at-least-once and unordered, so everything here is
//! idempotent: each delivery is recorded in `platform_events` in the same
//! transaction as its effects, and each effect is itself safe to repeat.
//!
//! | Event | Effect |
//! |---|---|
//! | `org.deleted` | Every row this database holds for the org is deleted, its SDK keys stop resolving at once, and its tokens are refused. Only the dedupe marker and the tombstone survive. |
//! | `member.removed` | The user's cached token is refused for a few minutes, until the platform's own introspection stops vouching for it. Flags they changed keep their history. |
//! | `team.deleted` | Nothing: otto-flags has no team-scoped data. Acknowledged. |
//!
//! # Why requests take a lock
//!
//! A request that passed the tombstone check a moment before an `org.deleted`
//! was applied could still re-create rows after the purge. So every request
//! transaction opens with [`begin_live`], which takes the org's lifecycle lock
//! *shared* and re-checks the tombstones while holding it, and the cleanup takes
//! the same lock *exclusively* after writing its tombstone. A transaction
//! already in flight finishes first and is purged with everything else; one
//! that starts later sees the tombstone and is refused.

use otto_resource::webhook::{LifecycleEvent, WebhookEvent};
use otto_tenant::audit::Entry;
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::{Db, Tx};
use serde::Serialize;

use crate::audit::action;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    Applied {
        detail: serde_json::Value,
    },
    /// An earlier delivery already applied this event. The cleanup was re-run
    /// anyway (it is idempotent) to catch work that raced the first run.
    Duplicate,
    /// An event this version does not act on. Acknowledged, not failed:
    /// failing it would only make the platform retry it forever.
    Ignored,
}

/// How long a removed member is refused. Past the introspection cache's 60 s
/// the platform itself answers "inactive"; this only has to outlast that.
pub const REMOVED_MEMBER_TTL_SECS: i64 = 300;

/// The advisory-lock namespace for an org's lifecycle lock ("fllc").
const LIFECYCLE_LOCK: i32 = 0x666c_6c63;

/// Whether `user`'s token for `org` must be refused regardless of what the
/// platform's cached introspection says. An `Err` is a database failure and
/// must be answered `503`, not treated as "not revoked".
pub async fn revoked(db: &Db, org: OrgId, user: UserId) -> Result<bool> {
    let mut conn = db.pool().acquire().await?;
    revoked_on(&mut conn, org, Some(user)).await
}

async fn revoked_on(
    conn: &mut sqlx::PgConnection,
    org: OrgId,
    user: Option<UserId>,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deleted_orgs WHERE org_id = $1) \
             OR EXISTS (SELECT 1 FROM removed_members \
                        WHERE org_id = $1 AND user_id = $2 \
                          AND removed_at > now() - make_interval(secs => $3))",
    )
    .bind(org)
    .bind(user)
    .bind(REMOVED_MEMBER_TTL_SECS as f64)
    .fetch_one(conn)
    .await?)
}

/// Open a transaction pinned to `org` that no lifecycle cleanup can interleave
/// with, refused if the org was deleted or `user` just removed. `user` is
/// `None` for SDK requests, which act for an app rather than a person.
pub async fn begin_live(db: &Db, org: OrgId, user: Option<UserId>) -> Result<Tx<'static>> {
    let mut tx = db.begin(org).await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1, hashtext($2::text))")
        .bind(LIFECYCLE_LOCK)
        .bind(org)
        .execute(tx.conn())
        .await?;
    if revoked_on(tx.conn(), org, user).await? {
        return Err(Error::AccessRevoked);
    }
    Ok(tx)
}

async fn exclude_requests(tx: &mut Tx<'_>, org: OrgId) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::text))")
        .bind(LIFECYCLE_LOCK)
        .bind(org)
        .execute(tx.conn())
        .await?;
    Ok(())
}

/// Housekeeping: forget dedupe markers older than `keep_days` and expired
/// removed-member tombstones.
pub async fn sweep(db: &Db, keep_days: i32) -> Result<u64> {
    let events = sqlx::query(
        "DELETE FROM platform_events WHERE received_at < now() - make_interval(days => $1)",
    )
    .bind(keep_days)
    .execute(db.pool())
    .await?
    .rows_affected();
    let tombstones = sqlx::query(
        "DELETE FROM removed_members WHERE removed_at < now() - make_interval(secs => $1)",
    )
    .bind((REMOVED_MEMBER_TTL_SECS * 2) as f64)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(events + tombstones)
}

/// Apply one verified delivery.
pub async fn apply(db: &Db, event: &WebhookEvent) -> Result<Outcome> {
    match &event.event {
        LifecycleEvent::OrgDeleted { org_id } => org_deleted(db, event, (*org_id).into()).await,
        LifecycleEvent::MemberRemoved { org_id, user_id } => {
            member_removed(db, event, (*org_id).into(), (*user_id).into()).await
        }
        LifecycleEvent::TeamDeleted { org_id, .. } => {
            let mut tx = db.begin((*org_id).into()).await?;
            let first = first_delivery(&mut tx, event, "team.deleted").await?;
            tx.commit().await?;
            Ok(if first {
                Outcome::Ignored
            } else {
                Outcome::Duplicate
            })
        }
        LifecycleEvent::Unknown { kind } => {
            tracing::warn!(kind = %kind, event_id = %event.id, "ignoring unknown platform event");
            Ok(Outcome::Ignored)
        }
        // `LifecycleEvent` is #[non_exhaustive].
        _ => Ok(Outcome::Ignored),
    }
}

async fn first_delivery(tx: &mut Tx<'_>, event: &WebhookEvent, kind: &str) -> Result<bool> {
    let org = tx.org();
    let inserted = sqlx::query(
        "INSERT INTO platform_events (event_id, org_id, kind) VALUES ($1, $2, $3) \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event.id)
    .bind(org)
    .bind(kind)
    .execute(tx.conn())
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

async fn org_deleted(db: &Db, event: &WebhookEvent, org: OrgId) -> Result<Outcome> {
    // Tombstone first, on its own, so authentication and SDK keys refuse this
    // org from now on -- before, not as part of, the purge.
    sqlx::query("INSERT INTO deleted_orgs (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING")
        .bind(org)
        .execute(db.pool())
        .await?;

    let mut tx = db.begin(org).await?;
    exclude_requests(&mut tx, org).await?;

    // The audit trail is append-only to a pinned transaction, so it is deleted
    // unpinned, under the lock above.
    let audit = sqlx::query("DELETE FROM audit_events WHERE org_id = $1")
        .bind(org)
        .execute(db.pool())
        .await?
        .rows_affected();

    let first = first_delivery(&mut tx, event, "org.deleted").await?;

    // Children before parents. Every statement names the org (guard 1) in
    // addition to the pinned policy (guard 2); app_keys has no policy and
    // relies on the predicate alone.
    let mut deleted = serde_json::Map::new();
    for table in [
        "flag_errors",
        "flag_eval_hourly",
        "flag_versions",
        "feature_flags",
        "app_keys",
        "flag_apps",
        "usage_outbox",
    ] {
        let n = sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(org)
            .execute(tx.conn())
            .await?
            .rows_affected();
        deleted.insert(table.to_string(), n.into());
    }
    deleted.insert("audit_events".into(), audit.into());
    tx.commit().await?;

    tracing::info!(org = %org, event_id = %event.id, first, ?deleted, "purged a deleted org's data");
    Ok(if first {
        Outcome::Applied {
            detail: deleted.into(),
        }
    } else {
        Outcome::Duplicate
    })
}

async fn member_removed(
    db: &Db,
    event: &WebhookEvent,
    org: OrgId,
    user: UserId,
) -> Result<Outcome> {
    sqlx::query(
        "INSERT INTO removed_members (org_id, user_id) VALUES ($1, $2) \
         ON CONFLICT (org_id, user_id) DO UPDATE SET removed_at = now()",
    )
    .bind(org)
    .bind(user)
    .execute(db.pool())
    .await?;

    let mut tx = db.begin(org).await?;
    exclude_requests(&mut tx, org).await?;
    let first = first_delivery(&mut tx, event, "member.removed").await?;
    if first {
        tx.audit(
            Entry::new(action::MEMBER_REMOVED)
                .actor_label("platform")
                .target("user", user.to_string()),
        )
        .await?;
    }
    tx.commit().await?;

    tracing::info!(org = %org, user = %user, "member removed; their tokens are refused");
    Ok(if first {
        Outcome::Applied {
            detail: serde_json::json!({}),
        }
    } else {
        Outcome::Duplicate
    })
}
