//! otto-flags' own audit actions, written with `otto_tenant`'s `Tx::audit` in
//! the same transaction as the change.
//!
//! Flag changes are not here: every one of them is already recorded, with a
//! full snapshot, actor, and reason, in `flag_versions` (see [`crate::flags`]).
//! This trail is for what has no history table of its own: apps, keys, and the
//! platform's lifecycle events.
//!
//! Actions are dotted and stable because they are queried by prefix and end up
//! in customers' exports. Renaming one is a breaking change.

pub mod action {
    pub const APP_CREATED: &str = "flags.app.created";
    pub const APP_UPDATED: &str = "flags.app.updated";
    pub const APP_KEYS_ROTATED: &str = "flags.app.keys_rotated";
    pub const MEMBER_REMOVED: &str = "platform.member.removed";
}
