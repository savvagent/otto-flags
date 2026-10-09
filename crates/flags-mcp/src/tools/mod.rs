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

pub fn router() -> ToolRouter<Flags> {
    Flags::org_router() + Flags::apps_router() + Flags::flags_router() + Flags::insight_router()
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
];

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
}
