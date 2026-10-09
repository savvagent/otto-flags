//! Typed identifiers for otto-flags' own domain.
//!
//! `otto_tenant::OrgId` is reused as-is (it is the tenant boundary shared by
//! every otto-* service); the ids here are local to this database and follow
//! the same newtype-over-`Uuid` shape for the same reason `otto-tenant`'s
//! `ids.rs` gives: the compiler should refuse to pass a `FlagId` where a
//! `FlagAppId` belongs, since both are `Uuid` on the wire and the mix-up would
//! otherwise compile and run.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
        #[serde(transparent)]
        #[sqlx(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }

        // Written out rather than derived, matching otto_tenant::ids: over the
        // wire these are UUID strings, and an MCP client's generated schema
        // should say that rather than naming the wrapper.
        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn schema_id() -> std::borrow::Cow<'static, str> {
                concat!(module_path!(), "::", stringify!($name)).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "format": "uuid",
                    "description": $doc,
                })
            }

            fn inline_schema() -> bool {
                true
            }
        }
    };
}

uuid_id!(
    FlagAppId,
    "An application registered for flag management — the seam between \
     MCP-driven management and SDK-driven evaluation. Holds the SDK/server \
     keys and environment list a running app authenticates with."
);
uuid_id!(FlagId, "A feature flag, scoped to one FlagApp.");
uuid_id!(
    EvaluationId,
    "One recorded flag evaluation, the raw evidence rollups and risk \
     assessment are computed from."
);
