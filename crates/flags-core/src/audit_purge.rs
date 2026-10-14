//! Whether the audit purge can run, read from the catalog (#40).
//!
//! `org.deleted` purges a deleted org's audit trail through
//! `purge_deleted_org_audit(uuid)` (migration 0004, #21), a `SECURITY DEFINER`
//! function. Audit rows are under row-level security, so the function only
//! sees them when its owner is exempt: a superuser or a role with `BYPASSRLS`.
//! It refuses to run for any other owner (SQLSTATE 55000), because a purge
//! that sees no rows would delete nothing and report success.
//!
//! The owner is whoever ran the migration, which is the role the server
//! connects as. A deployment that connects as a role subject to RLS therefore
//! boots, serves, and fails its first `org.deleted`, long after the deploy that
//! caused it. A non-exempt owner is deliberately unsupported (the design's
//! section 3): there is no fallback purge path. Instead the server calls
//! [`check`] at startup and refuses to serve when [`PurgeReport::problems`] is
//! not empty, so the misconfiguration surfaces on the deploy that introduced
//! it and the previous release keeps serving.
//!
//! The check reads catalogs only, in a transaction it rolls back.

use otto_tenant::ids::OrgId;
use otto_tenant::Db;
use sqlx::PgConnection;

use crate::error::Result;

/// The function `org.deleted` purges a deleted org's audit trail through
/// (migration 0004, #21).
pub const FUNCTION: &str = "public.purge_deleted_org_audit(uuid)";

/// What the catalog says about [`FUNCTION`] and the role that will call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    /// The function's owner; `None` when the function does not exist.
    pub owner: Option<String>,
    pub owner_is_superuser: bool,
    pub owner_bypasses_rls: bool,
    /// `current_user` in a transaction shaped like the purge's own.
    pub tenant_role: String,
    /// Whether `tenant_role` may execute the function.
    pub executable: bool,
}

impl PurgeReport {
    /// Everything that would make `org.deleted` fail, in the order to fix it.
    /// Empty when the purge can run.
    ///
    /// ```
    /// use flags_core::audit_purge::PurgeReport;
    ///
    /// let report = PurgeReport {
    ///     owner: Some("postgres".into()),
    ///     owner_is_superuser: true,
    ///     owner_bypasses_rls: false,
    ///     tenant_role: "otto_app".into(),
    ///     executable: true,
    /// };
    /// assert!(report.problems().is_empty());
    /// ```
    pub fn problems(&self) -> Vec<String> {
        let Some(owner) = &self.owner else {
            return vec![
                "purge_deleted_org_audit(uuid) does not exist, so every org.deleted \
                         would fail: run migrations (0004) against this database before \
                         serving (with FLAGS_RUN_MIGRATIONS=false, migrate separately first)"
                    .to_string(),
            ];
        };
        let mut problems = Vec::new();
        if !self.owner_is_superuser && !self.owner_bypasses_rls {
            let mut message = format!(
                "purge_deleted_org_audit(uuid) is owned by {owner:?}, which is neither a \
                 superuser nor BYPASSRLS, so it cannot see the audit trail it purges and every \
                 org.deleted would fail (SQLSTATE 55000). Hand the function to a role that is \
                 exempt (ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO <exempt role>), \
                 or run migrations as one and serve with FLAGS_RUN_MIGRATIONS=false"
            );
            // BYPASSRLS on the tenant role itself would fail the tenant
            // isolation proof, so that remedy is only offered for another role.
            if *owner != self.tenant_role {
                message.push_str(&format!(
                    ", or give {owner:?} BYPASSRLS (ALTER ROLE {owner} BYPASSRLS)"
                ));
            }
            message.push_str("; see docs/deploy/fly.md");
            problems.push(message);
        }
        if !self.executable {
            let role = &self.tenant_role;
            problems.push(format!(
                "{role:?}, the role tenant transactions run as, may not execute \
                 purge_deleted_org_audit(uuid), so every org.deleted would fail: GRANT EXECUTE \
                 ON FUNCTION purge_deleted_org_audit(uuid) TO {role}"
            ));
        }
        problems
    }

    /// One line for the boot log when [`problems`](Self::problems) is empty.
    pub fn summary(&self) -> String {
        let (owner, exemption) = match &self.owner {
            None => ("missing".to_string(), "missing"),
            Some(owner) => (
                format!("{owner:?}"),
                if self.owner_is_superuser {
                    "superuser"
                } else {
                    "BYPASSRLS"
                },
            ),
        };
        format!(
            "audit purge ready: purge_deleted_org_audit(uuid) is owned by {owner} ({exemption}) \
             and executable by {:?}",
            self.tenant_role
        )
    }
}

/// Reads the catalog on `conn`, which should be in a transaction shaped like
/// the purge's own so `current_user` is the tenant role.
pub async fn inspect(conn: &mut PgConnection) -> Result<PurgeReport> {
    // The owner columns are NULL when the function is missing.
    let (owner, superuser, bypass, tenant_role, executable): (
        Option<String>,
        Option<bool>,
        Option<bool>,
        String,
        bool,
    ) = sqlx::query_as(
        "SELECT r.rolname::text, r.rolsuper, r.rolbypassrls, current_user::text, \
                COALESCE(has_function_privilege(p.oid, 'EXECUTE'), false) \
           FROM (SELECT to_regprocedure($1) AS oid) p \
           LEFT JOIN pg_catalog.pg_proc f ON f.oid = p.oid \
           LEFT JOIN pg_catalog.pg_roles r ON r.oid = f.proowner",
    )
    .bind(FUNCTION)
    .fetch_one(conn)
    .await
    .map_err(otto_tenant::Error::from)?;
    Ok(PurgeReport {
        owner,
        owner_is_superuser: superuser.unwrap_or(false),
        owner_bypasses_rls: bypass.unwrap_or(false),
        tenant_role,
        executable,
    })
}

/// [`inspect`] in a throwaway tenant transaction, which is rolled back.
pub async fn check(db: &Db) -> Result<PurgeReport> {
    let mut tx = db.begin(OrgId::new()).await?;
    let report = inspect(tx.conn()).await;
    tx.rollback().await?;
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> PurgeReport {
        PurgeReport {
            owner: Some("postgres".into()),
            owner_is_superuser: true,
            owner_bypasses_rls: false,
            tenant_role: "otto_app".into(),
            executable: true,
        }
    }

    fn non_exempt(owner: &str) -> PurgeReport {
        PurgeReport {
            owner: Some(owner.into()),
            owner_is_superuser: false,
            owner_bypasses_rls: false,
            ..ready()
        }
    }

    #[test]
    fn a_ready_report_has_no_problems() {
        assert!(ready().problems().is_empty());
    }

    #[test]
    fn a_bypassrls_owner_is_exempt() {
        let report = PurgeReport {
            owner_is_superuser: false,
            owner_bypasses_rls: true,
            ..ready()
        };
        assert!(report.problems().is_empty());
    }

    #[test]
    fn a_non_exempt_owner_gets_both_remedies() {
        let problems = non_exempt("migrator").problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        for needle in ["\"migrator\"", "55000", "OWNER TO", "ALTER ROLE"] {
            assert!(problems[0].contains(needle), "{needle}: {problems:?}");
        }
    }

    #[test]
    fn bypassrls_is_not_offered_for_the_tenant_role_itself() {
        let report = PurgeReport {
            tenant_role: "app".into(),
            ..non_exempt("app")
        };
        let problems = report.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("OWNER TO"), "{problems:?}");
        assert!(!problems[0].contains("ALTER ROLE"), "{problems:?}");
    }

    #[test]
    fn a_missing_function_is_the_only_problem() {
        let report = PurgeReport {
            owner: None,
            owner_is_superuser: false,
            owner_bypasses_rls: false,
            executable: false,
            ..ready()
        };
        let problems = report.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("does not exist"), "{problems:?}");
    }

    #[test]
    fn a_tenant_role_without_execute_is_reported() {
        let report = PurgeReport {
            executable: false,
            ..ready()
        };
        let problems = report.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("\"otto_app\""), "{problems:?}");
        assert!(problems[0].contains("GRANT EXECUTE"), "{problems:?}");
    }

    #[test]
    fn the_owner_is_reported_before_the_grant() {
        let report = PurgeReport {
            executable: false,
            ..non_exempt("migrator")
        };
        let problems = report.problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("migrator"), "{problems:?}");
        assert!(problems[1].contains("GRANT EXECUTE"), "{problems:?}");
    }

    #[test]
    fn the_summary_names_owner_exemption_and_role() {
        let summary = ready().summary();
        for needle in ["\"postgres\"", "superuser", "\"otto_app\""] {
            assert!(summary.contains(needle), "{needle}: {summary}");
        }
    }
}
