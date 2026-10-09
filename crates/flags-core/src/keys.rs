//! SDK credentials: minting, hashing, and resolving them to an app.
//!
//! Two kinds, matching what the SDKs in `packages/` already send:
//!
//! | Kind | Prefix | Where it lives |
//! |---|---|---|
//! | client | `sdk_` | Browser and mobile apps. Public by design. |
//! | server | `srv_` | Backend services. A secret. |
//!
//! Only a SHA-256 of each key is stored (`app_keys`), so a database read does
//! not yield a usable server key. The key is high-entropy random, so a plain
//! hash is enough; there is nothing for a slow KDF to protect.
//!
//! Resolution runs on the pool, unpinned, because the org is exactly what the
//! key is being resolved to. `app_keys` is outside row-level security for that
//! reason, and every statement here names the key hash explicitly.

use otto_tenant::ids::OrgId;
use otto_tenant::Db;
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::ids::FlagAppId;

pub const CLIENT_PREFIX: &str = "sdk_";
pub const SERVER_PREFIX: &str = "srv_";

/// How many characters of a key are kept in clear, to tell keys apart.
const SHOWN_PREFIX_LEN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum KeyKind {
    Client,
    Server,
}

impl KeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyKind::Client => "client",
            KeyKind::Server => "server",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            KeyKind::Client => CLIENT_PREFIX,
            KeyKind::Server => SERVER_PREFIX,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "client" => Some(KeyKind::Client),
            "server" => Some(KeyKind::Server),
            _ => None,
        }
    }
}

/// A freshly minted key, in clear. Exists only between minting and the
/// response that shows it.
#[derive(Debug, Clone)]
pub struct NewKey {
    pub kind: KeyKind,
    pub key: String,
}

impl NewKey {
    pub fn mint(kind: KeyKind) -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self {
            kind,
            key: format!("{}{}", kind.prefix(), hex::encode(bytes)),
        }
    }

    pub fn hash(&self) -> Vec<u8> {
        hash(&self.key)
    }

    pub fn shown_prefix(&self) -> String {
        self.key.chars().take(SHOWN_PREFIX_LEN).collect()
    }
}

pub fn hash(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

/// Whether a string is shaped like an SDK key at all. Lets the SDK surface
/// reject a misplaced OAuth token or a typo without a database round trip.
pub fn looks_like_key(s: &str) -> bool {
    (s.starts_with(CLIENT_PREFIX) || s.starts_with(SERVER_PREFIX))
        && s.len() == CLIENT_PREFIX.len() + 64
        && s[CLIENT_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

/// What a key opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyOwner {
    pub org_id: OrgId,
    pub app_id: FlagAppId,
    pub kind: KeyKind,
}

/// Resolve a presented key, or `None` if it is unknown, revoked by rotation, or
/// belongs to an org the platform has deleted.
pub async fn resolve(db: &Db, key: &str) -> Result<Option<KeyOwner>> {
    if !looks_like_key(key) {
        return Ok(None);
    }
    let row: Option<(uuid::Uuid, uuid::Uuid, String)> = sqlx::query_as(
        "SELECT k.org_id, k.app_id, k.kind FROM app_keys k \
         WHERE k.key_hash = $1 \
           AND NOT EXISTS (SELECT 1 FROM deleted_orgs d WHERE d.org_id = k.org_id)",
    )
    .bind(hash(key))
    .fetch_optional(db.pool())
    .await?;
    Ok(row.and_then(|(org, app, kind)| {
        Some(KeyOwner {
            org_id: org.into(),
            app_id: app.into(),
            kind: KeyKind::parse(&kind)?,
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_keys_carry_their_kind_and_are_recognised() {
        let c = NewKey::mint(KeyKind::Client);
        let s = NewKey::mint(KeyKind::Server);
        assert!(c.key.starts_with("sdk_"));
        assert!(s.key.starts_with("srv_"));
        assert!(looks_like_key(&c.key) && looks_like_key(&s.key));
        assert_ne!(c.key, NewKey::mint(KeyKind::Client).key);
        assert_eq!(c.shown_prefix().len(), 12);
    }

    #[test]
    fn other_strings_are_not_keys() {
        for bad in [
            "",
            "sdk_",
            "sdk_xyz",
            "otto_at_abcdef",
            &format!("pat_{}", "a".repeat(64)),
            &format!("sdk_{}", "g".repeat(64)),
        ] {
            assert!(!looks_like_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_hash_is_stable_and_does_not_contain_the_key() {
        let k = NewKey::mint(KeyKind::Server);
        assert_eq!(k.hash(), hash(&k.key));
        assert_eq!(k.hash().len(), 32);
    }
}
