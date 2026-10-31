//! Whether the audit purge can run, read from the catalog (#40).
//!
//! `org.deleted` purges a deleted org's audit trail through
//! `purge_deleted_org_audit(uuid)` (migration 0004, #21), a `SECURITY DEFINER`
//! function. Audit rows are under row-level security, so the function only
//! sees them when its owner is exempt: a superuser or a role with `BYPASSRLS`.
//! It refuses to run for any other owner (SQLSTATE 55000), because a purge
//! that sees no rows would delete nothing and report success.
//!
//! The owner is the role that ran migration 0004: by default the role the
//! server connects as, since migrations run at boot. A deployment migrating as
//! a role subject to RLS would boot, serve, and fail its first `org.deleted`,
//! long after the deploy that caused it. A non-exempt owner is deliberately
//! unsupported (section 3 of
//! `docs/specs/2026-10-14-purge-owner-startup-check-design.md`): there is no
//! fallback purge path. Instead the server calls [`check`] at startup and
//! refuses to serve when [`PurgeReport::problems`] is not empty, so the
//! misconfiguration surfaces on the deploy that introduced it, which rolls
//! back, rather than on a customer's deletion.
//!
//! The check reads catalogs only, in a transaction it rolls back.

use savvagent_tenant::ids::OrgId;
use savvagent_tenant::Db;
use sqlx::PgConnection;

use crate::error::Result;

/// The purge function's unqualified name: the one literal every name below,
/// and every statement the check suggests, is built from (#53, #98).
macro_rules! purge_name {
    () => {
        "purge_deleted_org_audit"
    };
}

/// The purge function's schema-qualified name: the one source for
/// [`FUNCTION`] and [`PURGE`], so the check and the call cannot drift (#53).
macro_rules! purge_function {
    () => {
        concat!("public.", purge_name!())
    };
}

/// The function `org.deleted` purges a deleted org's audit trail through
/// (migration 0004, #21). Schema-qualified because everything this database
/// owns lives in `public`; 0004 creates it unqualified, so if it landed
/// anywhere else the startup check reports it missing.
pub const FUNCTION: &str = concat!(purge_function!(), "(uuid)");

/// The function's bare name, for the catalog query and the `SET SCHEMA`
/// statement the missing-function problem suggests.
const NAME: &str = purge_name!();

/// The statement `org.deleted` purges through: [`FUNCTION`], named by schema
/// so `search_path` cannot resolve it to a same-named function elsewhere
/// (#53).
pub(crate) const PURGE: &str = concat!("SELECT ", purge_function!(), "($1)");

/// What the catalog says about [`FUNCTION`] and the role that will call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    /// The function's owner; `None` when the function does not exist.
    pub owner: Option<String>,
    /// The owner is a superuser (`rolsuper`), so RLS does not apply to it.
    pub owner_is_superuser: bool,
    /// The owner holds `BYPASSRLS` itself; like the function's own guard,
    /// membership in a role that holds it does not count.
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
            // Migrations will not re-run 0004 once it is recorded as applied,
            // so "run migrations" alone is wrong when 0004 ran but created the
            // function elsewhere (unqualified, it lands in the first schema on
            // the migrating role's search_path) or the function was dropped.
            // Moving it is safe: 0004 pins the function's own search_path and
            // names every table by schema, and owner and grants move with it.
            return vec![format!(
                "{FUNCTION} does not exist, so every org.deleted would fail. If migration \
                 0004 has not run, run migrations against this database before serving \
                 (with FLAGS_RUN_MIGRATIONS=false, migrate separately first). If 0004 is \
                 recorded as applied (version 4 in _sqlx_migrations), migrations will not \
                 re-run it. Find the function (SELECT quote_ident(n.nspname), \
                 pg_get_function_identity_arguments(p.oid), p.prosecdef FROM \
                 pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = \
                 p.pronamespace WHERE p.proname = '{NAME}'). If a row is 0004's (arguments \
                 purge_org uuid, SECURITY DEFINER, and a pg_get_functiondef(p.oid) body \
                 matching crates/flags-core/migrations/0004_audit_purge.sql), move it into public as \
                 a superuser (ALTER FUNCTION <schema>.{NAME}(uuid) SET SCHEMA public); \
                 otherwise run that file as a superuser with SET search_path = public \
                 first, so it does not land elsewhere again; see docs/deploy/fly.md"
            )];
        };
        let mut problems = Vec::new();
        // The same test the function body applies to its owner in 0004.
        if !self.owner_is_superuser && !self.owner_bypasses_rls {
            let mut message = format!(
                "{FUNCTION} is owned by {owner:?}, which is neither a \
                 superuser nor BYPASSRLS, so it cannot see the audit trail it purges and every \
                 org.deleted would fail (SQLSTATE 55000). Hand the function to a role that is \
                 exempt (ALTER FUNCTION {FUNCTION} OWNER TO <exempt role>), \
                 or run migrations as one and serve with FLAGS_RUN_MIGRATIONS=false"
            );
            // BYPASSRLS on the tenant role itself would fail the tenant
            // isolation proof, so that remedy is only offered for another role.
            if *owner != self.tenant_role {
                message.push_str(&format!(
                    ", or give {owner:?} BYPASSRLS (ALTER ROLE {} BYPASSRLS)",
                    quote_ident(owner)
                ));
            }
            message.push_str("; see docs/deploy/fly.md");
            problems.push(message);
        }
        if !self.executable {
            let role = &self.tenant_role;
            problems.push(format!(
                "{role:?}, the role tenant transactions run as, may not execute {FUNCTION}, \
                 so every org.deleted would fail: GRANT EXECUTE ON FUNCTION {FUNCTION} TO {}",
                quote_ident(role)
            ));
        }
        problems
    }

    /// One line for the boot log. Only meaningful once
    /// [`problems`](Self::problems) is empty; otherwise it still describes
    /// the facts truthfully but says "not ready".
    pub fn summary(&self) -> String {
        let owner = match &self.owner {
            None => "nobody (missing)".to_string(),
            Some(owner) => format!("{owner:?}"),
        };
        let exemption = if self.owner_is_superuser {
            "superuser"
        } else if self.owner_bypasses_rls {
            "BYPASSRLS"
        } else {
            "not exempt"
        };
        let state = if self.problems().is_empty() {
            "ready"
        } else {
            "not ready"
        };
        let executable = if self.executable {
            "executable"
        } else {
            "not executable"
        };
        format!(
            "audit purge {state}: {FUNCTION} is owned by {owner} \
             ({exemption}) and {executable} by {:?}",
            self.tenant_role
        )
    }
}

/// `name` as a SQL identifier, so a suggested statement names exactly this
/// role when pasted, whatever its case or characters.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
    .await?;
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
    let mut tx = crate::platform_events::begin_pinned(db, OrgId::new()).await?;
    // On an error the dropped transaction rolls back, and the inspect error
    // is the one worth reporting.
    let report = inspect(tx.conn()).await?;
    tx.rollback().await?;
    Ok(report)
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
        for needle in [
            "\"migrator\"",
            "55000",
            "OWNER TO",
            "ALTER ROLE \"migrator\" BYPASSRLS",
        ] {
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
    fn a_missing_function_explains_an_applied_migration() {
        let report = PurgeReport {
            owner: None,
            ..ready()
        };
        let problems = report.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        for needle in [
            FUNCTION,
            "recorded as applied",
            "proname = 'purge_deleted_org_audit'",
            "_sqlx_migrations",
            "0004_audit_purge.sql",
            "SET search_path = public",
        ] {
            assert!(problems[0].contains(needle), "{needle}: {problems:?}");
        }
        let advice = format!("ALTER FUNCTION <schema>.{NAME}(uuid) SET SCHEMA public");
        assert!(problems[0].contains(&advice), "{problems:?}");
    }

    #[test]
    fn suggested_statements_name_the_function_by_schema() {
        assert!(FUNCTION.starts_with("public."), "{FUNCTION}");
        let report = PurgeReport {
            executable: false,
            ..non_exempt("migrator")
        };
        let problems = report.problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        let alter = format!("ALTER FUNCTION {FUNCTION} OWNER TO");
        assert!(problems[0].contains(&alter), "{problems:?}");
        let grant = format!("GRANT EXECUTE ON FUNCTION {FUNCTION} TO");
        assert!(problems[1].contains(&grant), "{problems:?}");
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
    fn suggested_statements_quote_role_names() {
        let report = PurgeReport {
            executable: false,
            tenant_role: "App\"Role".into(),
            ..non_exempt("Migrator")
        };
        let problems = report.problems();
        assert!(
            problems[0].contains("ALTER ROLE \"Migrator\" BYPASSRLS"),
            "{problems:?}"
        );
        assert!(problems[1].contains("TO \"App\"\"Role\""), "{problems:?}");
    }

    #[test]
    fn the_summary_tells_the_truth_about_a_report_with_problems() {
        let report = PurgeReport {
            executable: false,
            ..non_exempt("migrator")
        };
        let summary = report.summary();
        for needle in ["not ready", "not exempt", "not executable"] {
            assert!(summary.contains(needle), "{needle}: {summary}");
        }
        let bypass = PurgeReport {
            owner_is_superuser: false,
            owner_bypasses_rls: true,
            ..ready()
        };
        assert!(
            bypass.summary().contains("(BYPASSRLS)"),
            "{}",
            bypass.summary()
        );
    }

    #[test]
    fn the_summary_names_owner_exemption_and_role() {
        let summary = ready().summary();
        assert!(summary.starts_with("audit purge ready: "), "{summary}");
        for needle in [
            FUNCTION,
            "\"postgres\"",
            "(superuser)",
            "executable by \"otto_app\"",
        ] {
            assert!(summary.contains(needle), "{needle}: {summary}");
        }
    }
}
