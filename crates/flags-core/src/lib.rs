//! `flags-core` — otto-flags' own domain crate: flag apps, feature flags, and
//! evaluation ingestion, scoped by the shared `org_id` and built on
//! `otto_tenant::{Db, Tx}` for row-level-security-enforced tenant isolation.
//!
//! This is otto-flags' *own* Postgres database, physically separate from
//! otto-platform's identity/auth/billing database — see
//! `docs/specs/2026-09-15-otto-flags-design.md` §3. `otto_tenant::Db` and
//! `Tx` are reused unmodified from otto-platform (same crate, same isolation
//! proof); only the migrations and the query methods here are otto-flags'
//! own. Because `Db`/`Tx` are defined in a crate this one depends on rather
//! than owns, their query methods are added as **extension traits**
//! ([`apps::AppsExt`], [`flags::FlagsExt`], [`evaluations::EvaluationsExt`])
//! rather than inherent `impl` blocks — the same shape `otto-core` uses on
//! top of `otto-tenant`. Import the trait alongside `Tx` to call its methods:
//!
//! ```no_run
//! use flags_core::apps::AppsExt;
//! use otto_tenant::{Db, ids::OrgId};
//!
//! # async fn example(db: &Db, org: OrgId) -> flags_core::error::Result<()> {
//! let mut tx = db.begin(org).await?;
//! let app = tx.create_app("my-app", &["production".to_string()]).await?;
//! tx.commit().await?;
//! # Ok(())
//! # }
//! ```
//!
//! Migrations live in `migrations/` and are **not** run by
//! `otto_tenant::Db::migrate()` — that method's `sqlx::migrate!("./migrations")`
//! is fixed at compile time to otto-tenant's own migrations directory. A
//! binary embedding this crate's schema (see `flags-server`) runs
//! `sqlx::migrate!` pointed at this crate's `migrations/` directory directly.

pub mod apps;
pub mod error;
pub mod evaluations;
pub mod flags;
pub mod ids;

pub use error::{Error, Result};
