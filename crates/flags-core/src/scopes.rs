//! The scopes otto-flags' resource server understands.
//!
//! Registered with the platform (`otto-platform-server resource register
//! --scopes …`, see docs/deploy/fly.md), advertised in
//! `/.well-known/oauth-protected-resource`, and checked by every tool. One
//! list, so the three cannot drift.

/// What this resource server is called on a consent screen and in its metadata.
pub const RESOURCE_NAME: &str = "otto-flags";

/// Read apps, flags, history, and health; dry-run evaluation.
pub const FLAGS_READ: &str = "flags:read";
/// Create, change, roll out, roll back, and archive flags.
pub const FLAGS_WRITE: &str = "flags:write";
/// Create apps and issue or rotate their SDK keys. Also requires the caller to
/// be an org owner or admin.
pub const APPS_ADMIN: &str = "apps:admin";

pub const KNOWN: &[&str] = &[FLAGS_READ, FLAGS_WRITE, APPS_ADMIN];

/// Granted when a client asks for nothing in particular. Read-only, as in
/// otto-factory: a client that wants to change anything has to ask, and the
/// person sees it on the consent screen.
pub const DEFAULT: &[&str] = &[FLAGS_READ];

#[cfg(test)]
mod tests {
    use super::*;

    /// These strings are carried by issued tokens and registered at the
    /// platform. Changing one breaks every existing grant.
    #[test]
    fn the_scope_list_is_stable() {
        assert_eq!(KNOWN, ["flags:read", "flags:write", "apps:admin"]);
        assert!(DEFAULT.iter().all(|s| KNOWN.contains(s)));
    }
}
