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

    #[error(
        "{feature} is not included in this organization's {plan} plan, so this was refused. \
         Upgrade at {upgrade_url}"
    )]
    FeatureNotInPlan {
        feature: String,
        plan: String,
        upgrade_url: String,
    },

    /// The platform could not confirm the entitlement; fails closed. The text
    /// is ours, never the platform's.
    #[error(
        "the otto platform could not confirm that this organization's plan includes {feature}; \
         nothing was changed. Retry shortly."
    )]
    EntitlementUnavailable { feature: String },

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
            Error::FeatureNotInPlan { .. } => "feature_not_in_plan",
            Error::EntitlementUnavailable { .. } | Error::Platform(_) => "platform_unavailable",
            Error::Tenant(t) => t.code(),
            Error::Db(_) => "internal_error",
        }
    }

    /// Whether repeating the same call unchanged could succeed.
    pub fn retriable(&self) -> bool {
        match self {
            Error::Db(_) | Error::EntitlementUnavailable { .. } => true,
            Error::Platform(p) => p.is_retriable(),
            Error::Tenant(otto_tenant::Error::Db(_)) => true,
            _ => false,
        }
    }

    /// Whether this failure is the kind that passes on its own: contention
    /// (serialization failure, deadlock, a lock or statement timeout), a
    /// connection or pool problem, or the platform being unreachable. Narrower
    /// than [`Error::retriable`], which tells an agent any database error is
    /// worth one more try: a background job retrying on a schedule must not
    /// keep retrying a constraint violation or a missing table.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Db(e) | Error::Tenant(otto_tenant::Error::Db(e)) => db_is_transient(e),
            Error::EntitlementUnavailable { .. } => true,
            Error::Platform(p) => p.is_retriable(),
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

    /// A text-free description of this failure for logs above debug: a fixed
    /// reason we chose, an HTTP status for a platform failure, the SQLSTATE for
    /// an error Postgres raised, and the connect/timeout/other sub-kind of a
    /// platform transport failure (`reason`, `status`, `sqlstate`, `transport`).
    /// Database and platform text can carry SQL, schema names, response bodies,
    /// and URLs, so it is logged only at debug; this is what an operator reads
    /// at `info`.
    pub fn failure_reason(&self) -> Failure {
        match self {
            Error::Db(e) | Error::Tenant(otto_tenant::Error::Db(e)) => db_failure_reason(e),
            Error::Platform(p) => platform_failure_reason(p),
            _ => Failure::reason(self.code()),
        }
    }
}

/// A text-free description of a failure, for logs above debug (#29, #35, #43).
///
/// Every field is either a name this crate chose or a value with no room for
/// text: an HTTP status, a validated five-character SQLSTATE. Database and
/// platform text can carry SQL, schema names, response bodies, and URLs, so it
/// is logged only at debug. Log sites write the fields that apply to the error
/// they hold as `reason`, `status`, `sqlstate`, and `transport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    /// A fixed name for the kind of failure.
    pub reason: &'static str,
    /// The HTTP status the platform answered with.
    pub status: Option<u16>,
    /// The SQLSTATE, when Postgres raised the error.
    pub sqlstate: Option<Sqlstate>,
    /// For a platform transport failure: `connect`, `timeout`, or `other`.
    pub transport: Option<&'static str>,
}

impl Failure {
    /// A failure known only by its reason.
    pub const fn reason(reason: &'static str) -> Self {
        Self {
            reason,
            status: None,
            sqlstate: None,
            transport: None,
        }
    }

    /// The SQLSTATE as a log field value.
    pub fn sqlstate(&self) -> Option<&str> {
        self.sqlstate.as_ref().map(Sqlstate::as_str)
    }
}

/// A Postgres SQLSTATE: five ASCII uppercase letters or digits, checked on
/// construction so nothing but the code can reach a log line through it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sqlstate([u8; 5]);

impl Sqlstate {
    /// `None` unless `code` is exactly five ASCII uppercase letters or digits.
    pub fn parse(code: &str) -> Option<Self> {
        let bytes: [u8; 5] = code.as_bytes().try_into().ok()?;
        bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            .then_some(Self(bytes))
    }

    /// The five-character code, for a log field.
    pub fn as_str(&self) -> &str {
        // Checked ASCII on construction.
        std::str::from_utf8(&self.0).expect("ascii sqlstate")
    }
}

impl std::fmt::Debug for Sqlstate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sqlstate({})", self.as_str())
    }
}

/// [`Error::failure_reason`] for a platform failure the caller holds unwrapped.
pub fn platform_failure_reason(e: &otto_resource::Error) -> Failure {
    use otto_resource::Error as E;
    match e {
        E::Status { status, .. } => Failure {
            status: Some(*status),
            ..Failure::reason("http_status")
        },
        E::Transport(_) => Failure {
            transport: transport_kind(e),
            ..Failure::reason("transport")
        },
        E::Decode(_) => Failure::reason("decode"),
        E::Unauthorized => Failure::reason("unauthorized"),
        E::NotFound => Failure::reason("not_found"),
        E::InvalidUrl(_) => Failure::reason("invalid_url"),
    }
}

/// The connect/timeout/other sub-kind of a platform transport failure, from
/// the transport error's own predicates (no text), `None` for any other variant.
/// The platform client sets only a total request timeout, so a connect that
/// hangs until it fires is `timeout`; `connect` is a refusal, DNS, or TLS failure.
fn transport_kind(e: &otto_resource::Error) -> Option<&'static str> {
    match e {
        otto_resource::Error::Transport(t) => Some(if t.is_connect() {
            "connect"
        } else if t.is_timeout() {
            "timeout"
        } else {
            "other"
        }),
        _ => None,
    }
}

/// [`Error::failure_reason`] for a database failure the caller holds as a bare
/// `sqlx::Error`: the sqlx variant, or for an error Postgres raised, the
/// condition name of a common SQLSTATE (`database_error` for the rest), plus
/// the validated SQLSTATE itself: never the message, which can quote values and
/// schema.
pub fn db_failure_reason(e: &sqlx::Error) -> Failure {
    let sqlstate = match e {
        sqlx::Error::Database(db) => db.code().and_then(|c| Sqlstate::parse(&c)),
        _ => None,
    };
    Failure {
        sqlstate,
        ..Failure::reason(db_reason(e))
    }
}

fn db_reason(e: &sqlx::Error) -> &'static str {
    match e {
        sqlx::Error::PoolTimedOut => "pool_timed_out",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::Io(_) => "io",
        sqlx::Error::Tls(_) => "tls",
        sqlx::Error::Protocol(_) => "protocol",
        sqlx::Error::RowNotFound => "row_not_found",
        // `maintenance::begin` refusing a role exempt from row-level security.
        sqlx::Error::Configuration(_) => "configuration",
        sqlx::Error::TypeNotFound { .. }
        | sqlx::Error::ColumnIndexOutOfBounds { .. }
        | sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnDecode { .. }
        | sqlx::Error::Decode(_) => "decode",
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some("40001") => "serialization_failure",
            Some("40P01") => "deadlock_detected",
            Some("55P03") => "lock_not_available",
            Some("57014") => "query_canceled",
            Some("57P01") => "admin_shutdown",
            Some("57P02") => "crash_shutdown",
            Some("57P03") => "cannot_connect_now",
            Some("25006") => "read_only_sql_transaction",
            Some("25P02") => "in_failed_sql_transaction",
            Some("42501") => "insufficient_privilege",
            Some("42P01") => "undefined_table",
            Some("42703") => "undefined_column",
            Some(c) if c.starts_with("23") => "integrity_constraint_violation",
            Some(c) if c.starts_with("08") => "connection_exception",
            Some(c) if c.starts_with("53") => "insufficient_resources",
            Some(c) if c.starts_with("57") => "operator_intervention",
            Some(c) if c.starts_with("25") => "invalid_transaction_state",
            _ => "database_error",
        },
        _ => "other",
    }
}

/// SQLSTATEs that a later attempt can avoid: serialization_failure,
/// deadlock_detected, lock_not_available (`lock_timeout`), and query_canceled
/// (`statement_timeout`).
const TRANSIENT_SQLSTATES: &[&str] = &["40001", "40P01", "55P03", "57014"];

fn db_is_transient(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => true,
        sqlx::Error::Database(db) => db
            .code()
            .is_some_and(|code| TRANSIENT_SQLSTATES.contains(&code.as_ref())),
        _ => false,
    }
}

/// Map a unique violation on `constraint` to `to`, anything else to [`Error::Db`].
pub(crate) fn on_unique(e: sqlx::Error, to: impl FnOnce() -> Error) -> Error {
    match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => to(),
        _ => Error::Db(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    /// A database error with a chosen SQLSTATE: sqlx's own Postgres error
    /// cannot be built outside sqlx.
    #[derive(Debug)]
    struct FakeDbError(&'static str);

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "sqlstate {}", self.0)
        }
    }

    impl std::error::Error for FakeDbError {}

    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            "test"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn db(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError(code)))
    }

    #[test]
    fn only_contention_cancellation_and_connection_failures_are_transient() {
        let io = || std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        for transient in [
            Error::Db(sqlx::Error::PoolTimedOut),
            Error::Db(sqlx::Error::PoolClosed),
            Error::Db(sqlx::Error::Io(io())),
            // serialization_failure, deadlock_detected, lock_not_available,
            // query_canceled (statement or lock timeout)
            Error::Db(db("40001")),
            Error::Db(db("40P01")),
            Error::Db(db("55P03")),
            Error::Db(db("57014")),
            Error::Tenant(otto_tenant::Error::Db(sqlx::Error::PoolTimedOut)),
            Error::Tenant(otto_tenant::Error::Db(db("40P01"))),
            Error::EntitlementUnavailable {
                feature: "auto_rollback".into(),
            },
        ] {
            assert!(transient.is_transient(), "{transient:?}");
        }
        for permanent in [
            // unique_violation, undefined_table, check_violation,
            // insufficient_privilege
            Error::Db(db("23505")),
            Error::Db(db("42P01")),
            Error::Db(db("23514")),
            Error::Db(db("42501")),
            Error::Db(sqlx::Error::RowNotFound),
            Error::Db(sqlx::Error::Protocol("bad".into())),
            Error::Tenant(otto_tenant::Error::Db(db("23505"))),
            Error::Tenant(otto_tenant::Error::Invalid("bad".into())),
            Error::AccessRevoked,
            Error::Invalid("bad".into()),
        ] {
            assert!(!permanent.is_transient(), "{permanent:?}");
        }
    }

    #[test]
    fn failure_reasons_name_the_kind_without_the_text() {
        let cases = [
            (Error::Db(sqlx::Error::PoolTimedOut), "pool_timed_out", None),
            (Error::Db(db("40P01")), "deadlock_detected", None),
            (Error::Db(db("42501")), "insufficient_privilege", None),
            (
                Error::Db(db("23505")),
                "integrity_constraint_violation",
                None,
            ),
            (Error::Db(db("40001")), "serialization_failure", None),
            (Error::Db(db("55P03")), "lock_not_available", None),
            (Error::Db(db("57014")), "query_canceled", None),
            (Error::Db(db("42P01")), "undefined_table", None),
            (Error::Db(db("42703")), "undefined_column", None),
            (Error::Db(db("08006")), "connection_exception", None),
            (Error::Db(db("53300")), "insufficient_resources", None),
            (Error::Db(db("XX000")), "database_error", None),
            (Error::Db(sqlx::Error::RowNotFound), "row_not_found", None),
            (
                Error::Db(sqlx::Error::Configuration("secret".into())),
                "configuration",
                None,
            ),
            (
                Error::Db(sqlx::Error::Protocol("secret".into())),
                "protocol",
                None,
            ),
            (
                Error::Tenant(otto_tenant::Error::Db(sqlx::Error::PoolClosed)),
                "pool_closed",
                None,
            ),
            (
                Error::Platform(otto_resource::Error::Status {
                    status: 503,
                    body: "secret".into(),
                }),
                "http_status",
                Some(503),
            ),
            (
                Error::Platform(otto_resource::Error::Unauthorized),
                "unauthorized",
                None,
            ),
            (
                Error::Tenant(otto_tenant::Error::Config("secret".into())),
                "internal_error",
                None,
            ),
            (Error::AccessRevoked, "access_revoked", None),
        ];
        for (e, reason, status) in cases {
            let f = e.failure_reason();
            assert_eq!((f.reason, f.status), (reason, status), "{e:?}");
        }
        use otto_resource::Error as P;
        for (e, reason) in [
            (P::NotFound, "not_found"),
            (P::Decode("secret".into()), "decode"),
            (P::InvalidUrl("secret".into()), "invalid_url"),
        ] {
            let f = platform_failure_reason(&e);
            assert_eq!((f.reason, f.status), (reason, None));
        }
    }

    #[test]
    fn sqlstate_is_logged_only_when_postgres_raised_it() {
        let f = Error::Db(db("57P01")).failure_reason();
        assert_eq!(f.reason, "admin_shutdown");
        assert_eq!(f.sqlstate(), Some("57P01"));
        assert_eq!(f.transport, None);

        let f = Error::Tenant(otto_tenant::Error::Db(db("22P02"))).failure_reason();
        assert_eq!(f.reason, "database_error");
        assert_eq!(f.sqlstate(), Some("22P02"));

        assert_eq!(
            Error::Db(sqlx::Error::PoolTimedOut)
                .failure_reason()
                .sqlstate(),
            None
        );
        assert_eq!(db_failure_reason(&db("bad code")).sqlstate(), None);
        assert_eq!(db_failure_reason(&db("57p01")).sqlstate(), None);
    }

    #[test]
    fn class_57_and_25_conditions_are_named() {
        for (code, reason) in [
            ("57P01", "admin_shutdown"),
            ("57P02", "crash_shutdown"),
            ("57P03", "cannot_connect_now"),
            ("57000", "operator_intervention"),
            ("57014", "query_canceled"),
            ("25006", "read_only_sql_transaction"),
            ("25P02", "in_failed_sql_transaction"),
            ("25001", "invalid_transaction_state"),
        ] {
            assert_eq!(db_failure_reason(&db(code)).reason, reason, "{code}");
        }
    }

    #[test]
    fn sqlstate_parse_accepts_only_five_uppercase_alphanumerics() {
        let s = Sqlstate::parse("40P01").expect("valid");
        assert_eq!(s.as_str(), "40P01");
        for bad in [
            "",
            "4000",
            "400011",
            "40p01",
            "40 01",
            "4000-",
            "4000\u{e9}",
            // Five bytes, one of them not ASCII: rejected by the byte check.
            "400\u{e9}",
        ] {
            assert!(Sqlstate::parse(bad).is_none(), "{bad:?}");
        }
    }

    async fn transport_failure(base: String, timeout: std::time::Duration) -> otto_resource::Error {
        let mut cfg = otto_resource::ClientConfig::new(base, "https://flags.test/mcp", "secret");
        cfg.timeout = timeout;
        let client = otto_resource::PlatformClient::new(cfg).expect("client");
        client
            .usage_status(uuid::Uuid::nil())
            .await
            .expect_err("the platform is not reachable")
    }

    #[tokio::test]
    async fn transport_failures_carry_a_sub_kind() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let long = std::time::Duration::from_secs(5);

        // connect: nothing listens on the port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let e = transport_failure(format!("http://127.0.0.1:{port}"), long).await;
        let f = platform_failure_reason(&e);
        assert_eq!(
            (f.reason, f.transport),
            ("transport", Some("connect")),
            "{e:?}"
        );

        // timeout: accepts and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let holder = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let e = transport_failure(
            format!("http://{addr}"),
            std::time::Duration::from_millis(200),
        )
        .await;
        let f = platform_failure_reason(&e);
        assert_eq!(
            (f.reason, f.transport),
            ("transport", Some("timeout")),
            "{e:?}"
        );
        holder.abort();

        // other: a response hyper cannot parse.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let garbage = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(b"NOT HTTP\r\n\r\n").await;
            }
        });
        let e = transport_failure(format!("http://{addr}"), long).await;
        let f = platform_failure_reason(&e);
        assert_eq!(
            (f.reason, f.transport),
            ("transport", Some("other")),
            "{e:?}"
        );
        garbage.abort();
    }

    #[test]
    fn retriable_keeps_its_meaning_for_mcp_callers() {
        // Any database error is still "retry the call" to an agent.
        assert!(Error::Db(db("23505")).retriable());
        assert!(Error::Db(sqlx::Error::PoolTimedOut).retriable());
    }
}
