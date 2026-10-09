//! `flags-core` — otto-flags' domain: flag apps and their SDK keys, flags and
//! their version history, the evaluation engine, telemetry, and the plumbing a
//! resource server of the otto platform needs (usage metering, lifecycle
//! events, change notifications).
//!
//! This is otto-flags' own database, separate from the platform's. Tenant
//! isolation comes from `otto_tenant::{Db, Tx}`: every tenant query is a
//! method on a [`otto_tenant::Tx`], which cannot exist without an org, and runs
//! under row-level security. Query methods are extension traits
//! ([`apps::AppsExt`], [`flags::FlagsExt`]) because `Tx` is defined in
//! otto-tenant. Request paths open their transactions with
//! [`platform_events::begin_live`], never `Db::begin` directly, so a deleted
//! org or removed member is refused.
//!
//! No SQL lives outside this crate.

pub mod apps;
pub mod audit;
pub mod error;
pub mod eval;
pub mod flags;
pub mod ids;
pub mod keys;
pub mod migrate;
pub mod notify;
pub mod platform_events;
pub mod scopes;
pub mod telemetry;
pub mod usage;

pub use error::{Error, Result};
pub use migrate::{migrate, MIGRATOR};
