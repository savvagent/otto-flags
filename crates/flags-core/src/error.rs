//! Errors from otto-flags' domain crate.
//!
//! Every message here may be read by an agent deciding its next call, so each
//! one says what was wrong and, where there is one, what to call instead. Wraps
//! `otto_tenant::Error` with `#[from]` rather than re-deriving its variants.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The caller's arguments are wrong; the message says how.
    #[error("{0}")]
    Invalid(String),

    /// No app by that name or id in this org. Lists the ones that exist.
    #[error("no flag app {name:?} in this organization; {hint}")]
    AppNotFound { name: String, hint: String },

    #[error("no flag {key:?} in app {app:?}; call list_flags to see the app's flags")]
    FlagNotFound { app: String, key: String },

    #[error("flag {key:?} has no version {version}; call flag_history to see its versions")]
    VersionNotFound { key: String, version: i32 },

    #[error("a flag app named {name:?} already exists in this organization")]
    DuplicateAppName { name: String },

    #[error("flag key {key:?} already exists in this app; use update_flag to change it")]
    DuplicateFlagKey { key: String },

    /// Optimistic concurrency: the caller read `expected`, someone else has
    /// since written `actual`.
    #[error(
        "flag {key:?} is at version {actual}, not {expected}: it changed since you read it. \
         Call get_flag, check the change is still what you want, and retry with the new version"
    )]
    VersionConflict {
        key: String,
        expected: i32,
        actual: i32,
    },

    /// The org was deleted, or the caller removed from it, since the
    /// credential was last checked.
    #[error("access to this organization has been revoked; sign in again")]
    AccessRevoked,

    #[error(
        "this organization has used all {included} operations its {plan} plan includes this \
         month ({used} so far), so {tool} was refused. Reads still work. Upgrade at {upgrade_url}"
    )]
    QuotaExceeded {
        tool: String,
        used: i64,
        included: i64,
        plan: String,
        upgrade_url: String,
    },

    #[error(transparent)]
    Platform(#[from] otto_resource::Error),

    #[error(transparent)]
    Tenant(#[from] otto_tenant::Error),

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl Error {
    /// A stable, machine-readable code. Agents branch on this, never on text.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Invalid(_) => "invalid_argument",
            Error::AppNotFound { .. } => "app_not_found",
            Error::FlagNotFound { .. } => "flag_not_found",
            Error::VersionNotFound { .. } => "version_not_found",
            Error::DuplicateAppName { .. } => "duplicate_app_name",
            Error::DuplicateFlagKey { .. } => "duplicate_flag_key",
            Error::VersionConflict { .. } => "version_conflict",
            Error::AccessRevoked => "access_revoked",
            Error::QuotaExceeded { .. } => "quota_exceeded",
            Error::Platform(_) => "platform_unavailable",
            Error::Tenant(t) => t.code(),
            Error::Db(_) => "internal_error",
        }
    }

    /// Whether repeating the same call unchanged could succeed.
    pub fn retriable(&self) -> bool {
        match self {
            Error::Db(_) => true,
            Error::Platform(p) => p.is_retriable(),
            Error::Tenant(otto_tenant::Error::Db(_)) => true,
            _ => false,
        }
    }

    /// Whether this is the server's fault (and its text must not reach a
    /// caller) rather than a description of the caller's request.
    pub fn is_internal(&self) -> bool {
        match self {
            Error::Db(_) | Error::Platform(_) => true,
            Error::Tenant(t) => !matches!(
                t,
                otto_tenant::Error::Invalid(_) | otto_tenant::Error::OrgNotFound(_)
            ),
            _ => false,
        }
    }
}

/// Map a unique violation on `constraint` to `to`, anything else to [`Error::Db`].
pub(crate) fn on_unique(e: sqlx::Error, to: impl FnOnce() -> Error) -> Error {
    match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => to(),
        _ => Error::Db(e),
    }
}
