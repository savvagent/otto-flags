//! Flag-change notifications, for SDKs holding a stream open.
//!
//! A flag write calls [`flag_changed`] inside its own transaction, which issues
//! `pg_notify`. Postgres delivers a notification only when the transaction
//! commits, so a rolled-back change is never announced, and every replica
//! hears every change. [`Listener`] is one `LISTEN` connection per process,
//! fanned out to subscribers through a broadcast channel.
//!
//! The payload is deliberately small (ids, key, version, kind): a notification
//! is capped at 8000 bytes and a flag's state is not. A subscriber that needs
//! the state reads it.

use std::time::Duration;

use otto_tenant::ids::OrgId;
use otto_tenant::Tx;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::{broadcast, watch};

use crate::error::Result;
use crate::flags::FeatureFlag;
use crate::ids::FlagAppId;

pub const CHANNEL: &str = "flag_changes";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    #[serde(rename = "flag.created")]
    Created,
    #[serde(rename = "flag.updated")]
    Updated,
    /// Archived. SDKs know this event as a deletion.
    #[serde(rename = "flag.deleted")]
    Deleted,
}

impl Kind {
    pub fn event_name(self) -> &'static str {
        match self {
            Kind::Created => "flag.created",
            Kind::Updated => "flag.updated",
            Kind::Deleted => "flag.deleted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagChange {
    pub kind: Kind,
    pub org_id: OrgId,
    pub app_id: FlagAppId,
    pub key: String,
    pub version: i32,
}

/// Announce a change once `tx` commits.
pub async fn flag_changed(tx: &mut Tx<'_>, flag: &FeatureFlag, kind: Kind) -> Result<()> {
    let change = FlagChange {
        kind,
        org_id: flag.org_id,
        app_id: flag.app_id,
        key: flag.key.clone(),
        version: flag.version,
    };
    let payload = serde_json::to_string(&change).expect("a FlagChange always serializes");
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(payload)
        .execute(tx.conn())
        .await?;
    Ok(())
}

/// The process's one `LISTEN` connection and its subscribers.
#[derive(Clone)]
pub struct Listener {
    tx: broadcast::Sender<FlagChange>,
}

impl Listener {
    /// Start listening. The task reconnects on its own after a lost connection
    /// (sqlx's `PgListener` does this transparently); notifications sent while
    /// it was disconnected are lost, which a subscriber tolerates because the
    /// SDKs also poll.
    pub fn spawn(pool: PgPool, mut shutdown: watch::Receiver<bool>) -> Self {
        let (tx, _) = broadcast::channel(1024);
        let sender = tx.clone();
        tokio::spawn(async move {
            loop {
                let mut listener = match PgListener::connect_with(&pool).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not open the flag-change listener; retrying");
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                            _ = shutdown.changed() => return,
                        }
                    }
                };
                if let Err(e) = listener.listen(CHANNEL).await {
                    tracing::warn!(error = %e, "could not LISTEN for flag changes; retrying");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                tracing::info!("listening for flag changes");
                loop {
                    tokio::select! {
                        n = listener.recv() => match n {
                            Ok(n) => match serde_json::from_str::<FlagChange>(n.payload()) {
                                // No receivers is fine: nobody is streaming.
                                Ok(change) => { let _ = sender.send(change); }
                                Err(e) => tracing::warn!(error = %e, "unreadable flag-change notification"),
                            },
                            Err(e) => {
                                tracing::warn!(error = %e, "flag-change listener lost its connection");
                                break;
                            }
                        },
                        _ = shutdown.changed() => return,
                    }
                }
            }
        });
        Self { tx }
    }

    /// A listener nothing feeds, for tests and for a server built without one.
    pub fn detached() -> Self {
        Self {
            tx: broadcast::channel(16).0,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<FlagChange> {
        self.tx.subscribe()
    }
}
