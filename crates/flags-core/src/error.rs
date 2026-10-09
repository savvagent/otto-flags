//! Errors from otto-flags' domain crate.
//!
//! Wraps `otto_tenant::Error` with `#[from]` rather than re-deriving its
//! variants, the same layering `otto-core` uses on top of `otto-tenant` — see
//! that crate's `error.rs` doc comment.

use crate::ids::{FlagAppId, FlagId};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),

    #[error("flag app {0} not found")]
    FlagAppNotFound(FlagAppId),

    #[error("flag {0} not found")]
    FlagNotFound(FlagId),

    /// A `feature_flags.key` that already exists for this app. Unique per
    /// `(app_id, key)`, enforced by the migration's constraint; this variant
    /// exists so a duplicate create reports as a domain error rather than a
    /// raw Postgres unique-violation leaking out of `flags-core`.
    #[error("flag key {key:?} already exists for this app")]
    DuplicateFlagKey { key: String },

    #[error(transparent)]
    Tenant(#[from] otto_tenant::Error),

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::Invalid(_) => "invalid_argument",
            Error::FlagAppNotFound(_) => "flag_app_not_found",
            Error::FlagNotFound(_) => "flag_not_found",
            Error::DuplicateFlagKey { .. } => "duplicate_flag_key",
            Error::Tenant(_) => "tenant_error",
            Error::Db(_) => "internal_error",
        }
    }
}
