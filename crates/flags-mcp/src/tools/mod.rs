//! The tool surface, split by domain. `rmcp`'s [`ToolRouter`] adds, so the
//! per-file routers are combined in [`router`].
//!
//! **Naming things.** Agents name an app by its name (or id) and a flag by its
//! key within that app. An unknown name is an error that lists what exists;
//! nothing falls back to a default app, because changing a flag in an app
//! nobody meant is an expensive, silent failure and an error is a cheap one.
//!
//! **Output shape.** Every result is an object with named fields, never a bare
//! array: MCP requires an object root for structured output, and an envelope
//! can grow without breaking callers. See [`out`].

use rmcp::handler::server::tool::ToolRouter;

use crate::server::Flags;

pub mod apps;
pub mod flags;
pub mod insight;
pub mod org;
pub mod out;
pub mod rollback;

pub fn router() -> ToolRouter<Flags> {
    Flags::org_router()
        + Flags::apps_router()
        + Flags::flags_router()
        + Flags::insight_router()
        + Flags::rollback_router()
}

/// Every tool name, in one place for tests and documentation.
pub const NAMES: &[&str] = &[
    "whoami",
    "usage",
    "list_apps",
    "create_app",
    "update_app",
    "rotate_app_keys",
    "list_flags",
    "get_flag",
    "create_flag",
    "update_flag",
    "set_flag_environment",
    "archive_flag",
    "restore_flag",
    "rollback_flag",
    "flag_history",
    "evaluate_flag",
    "flag_health",
    "set_rollback_policy",
    "remove_rollback_policy",
    "list_rollback_policies",
    "propose_rollback",
    "rollback_events",
];

/// Tools that only read. The rule: no write scope is required and no state
/// changes. Every tool but `propose_rollback` records a usage row through
/// `Flags::charge`, but a read's row is not billable. `readOnlyHint = true` lets connector clients run these
/// without asking. `propose_rollback` is a dry run and belongs here.
pub const READ_ONLY: &[&str] = &[
    "whoami",
    "usage",
    "list_apps",
    "list_flags",
    "get_flag",
    "flag_history",
    "evaluate_flag",
    "flag_health",
    "list_rollback_policies",
    "propose_rollback",
    "rollback_events",
];

/// Writes that can switch off or alter live evaluation, or delete something
/// (`destructiveHint = true`). Every write except the two creations qualifies:
/// each replaces existing state, and a client should confirm before running one.
/// `create_app` and `create_flag` only add, so they are not listed.
pub const DESTRUCTIVE: &[&str] = &[
    "update_app",
    "rotate_app_keys",
    "update_flag",
    "set_flag_environment",
    "archive_flag",
    "restore_flag",
    "rollback_flag",
    "set_rollback_policy",
    "remove_rollback_policy",
];

/// Writes whose repeat has no further effect (`idempotentHint = true`). This
/// excludes every billable tool, since each call records usage: a repeat is
/// another charge. Of the writes, only `remove_rollback_policy` is not
/// billable, and removing an absent policy deletes nothing and writes no audit
/// row; only its non-billable usage row repeats.
pub const IDEMPOTENT: &[&str] = &["remove_rollback_policy"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_tool_is_routed_and_metered_consistently() {
        let routed: Vec<String> = router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        let mut expected: Vec<String> = NAMES.iter().map(|s| s.to_string()).collect();
        let mut got = routed.clone();
        expected.sort();
        got.sort();
        assert_eq!(got, expected);
        for billable in flags_core::usage::BILLABLE {
            assert!(
                NAMES.contains(billable),
                "{billable} is billed but is not a tool"
            );
        }
    }

    #[test]
    fn every_tool_is_annotated() {
        for tool in router().list_all() {
            let name = tool.name.to_string();
            let Some(a) = tool.annotations else {
                panic!("{name} has no annotations");
            };
            let Some(read_only) = a.read_only_hint else {
                panic!("{name} has no read_only_hint");
            };
            if !read_only {
                assert!(a.destructive_hint.is_some(), "{name}: no destructive_hint");
                assert!(a.idempotent_hint.is_some(), "{name}: no idempotent_hint");
            }
        }
    }

    #[test]
    fn annotations_match_the_pinned_sets() {
        for set in [READ_ONLY, DESTRUCTIVE, IDEMPOTENT] {
            for name in set {
                assert!(NAMES.contains(name), "{name} is pinned but is not a tool");
            }
        }
        for name in DESTRUCTIVE.iter().chain(IDEMPOTENT) {
            assert!(
                !READ_ONLY.contains(name),
                "{name} is both a read and a write"
            );
        }
        for tool in router().list_all() {
            let name = tool.name.to_string();
            let n = name.as_str();
            let Some(a) = tool.annotations else {
                panic!("{name} has no annotations");
            };
            assert_eq!(a.read_only_hint, Some(READ_ONLY.contains(&n)), "{name}");
            if READ_ONLY.contains(&n) {
                assert_eq!(a.destructive_hint, None, "{name}");
                assert_eq!(a.idempotent_hint, None, "{name}");
            } else {
                assert_eq!(a.destructive_hint, Some(DESTRUCTIVE.contains(&n)), "{name}");
                assert_eq!(a.idempotent_hint, Some(IDEMPOTENT.contains(&n)), "{name}");
            }
        }
    }

    /// Cross-checks the classification against the code. The source scan
    /// matches names, not behaviour: it catches a read-only tool that checks
    /// a write scope or admin role, or a write that checks none, in its own
    /// body (archive and restore delegate to `set_archived`).
    #[test]
    fn read_only_tools_change_nothing() {
        for name in READ_ONLY {
            assert!(
                !flags_core::usage::BILLABLE.contains(name),
                "{name} is read-only but billable"
            );
        }
        let sources = [
            ("org.rs", include_str!("org.rs")),
            ("apps.rs", include_str!("apps.rs")),
            ("flags.rs", include_str!("flags.rs")),
            ("insight.rs", include_str!("insight.rs")),
            ("rollback.rs", include_str!("rollback.rs")),
        ];
        for name in NAMES {
            let head = format!("pub async fn {name}(");
            let (file, body) = sources
                .iter()
                .find_map(|(file, src)| {
                    let start = src.find(&head)? + head.len();
                    let rest = &src[start..];
                    let Some(end) = rest.find("\n    }\n") else {
                        panic!("{name} in {file}: no end of body found");
                    };
                    Some((*file, &rest[..end]))
                })
                .unwrap_or_else(|| panic!("{name}: not found in any tool source"));
            assert!(!body.is_empty(), "{name} in {file}: empty body");
            assert!(
                !body.contains("\n    pub ")
                    && !body.contains("\n    async fn ")
                    && !body.contains("\n    fn "),
                "{name} in {file}: body runs into the next function"
            );
            let writes = ["FLAGS_WRITE", "APPS_ADMIN", "require_admin", "set_archived"];
            if READ_ONLY.contains(name) {
                for w in writes {
                    assert!(
                        !body.contains(w),
                        "{name} in {file}: read-only but uses {w}"
                    );
                }
            } else {
                // archive_flag and restore_flag delegate their FLAGS_WRITE check
                // to `Flags::set_archived`.
                assert!(
                    ["FLAGS_WRITE", "APPS_ADMIN", "set_archived"]
                        .iter()
                        .any(|w| body.contains(w)),
                    "{name} in {file}: a write that checks no write scope"
                );
            }
        }
    }
}
