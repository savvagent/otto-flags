//! otto-flags' own audit actions, written with `savvagent_tenant`'s `Tx::audit` in
//! the same transaction as the change.
//!
//! Flag changes are not here: every one of them is already recorded, with a
//! full snapshot, actor, and reason, in `flag_versions` (see [`crate::flags`]),
//! for at least [`crate::flags::VERSION_RETENTION_DAYS`] days, and the newest
//! [`crate::flags::MIN_RETAINED_VERSIONS`] of each flag for good. This trail
//! is for what has no history table of its own: apps, keys, and the
//! platform's lifecycle events.
//!
//! Actions are dotted and stable because they are queried by prefix and end up
//! in customers' exports. Renaming one is a breaking change.

pub mod action {
    pub const APP_CREATED: &str = "flags.app.created";
    pub const APP_UPDATED: &str = "flags.app.updated";
    pub const APP_KEYS_ROTATED: &str = "flags.app.keys_rotated";
    pub const ROLLBACK_POLICY_SET: &str = "flags.rollback_policy.set";
    pub const ROLLBACK_POLICY_REMOVED: &str = "flags.rollback_policy.removed";
    pub const MEMBER_REMOVED: &str = "platform.member.removed";
}
