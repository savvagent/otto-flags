# Rust SDK Context-Keyed Cache Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The Rust SDK's evaluation cache returns a cached result only for the flag and context
that produced it.

**Architecture:** `Cache` becomes a two-level map, flag key then canonical context key, with a
per-flag size bound; `FlagClient::evaluate` prepares the context before the lookup and uses the
same context for the key and the request.

**Tech Stack:** Rust 2021, reqwest, serde_json, tokio, wiremock (tests).

**Ticket:** #91

**Spec:** `docs/specs/2026-10-15-rust-sdk-context-cache-design.md`

## Global Constraints

- Gates, run in `packages/rust-server` with `CARGO_BUILD_JOBS=3` and
  `TMPDIR=$PWD/target/tmp` (mkdir first): `cargo fmt --check`;
  `cargo clippy --locked --all-targets -- -D warnings`; `cargo test --locked`; the same three in
  `examples/rust-server` (path dependent of this crate).
- Evidence: pass counts per test binary; a test that did not run is not a pass.
- Invariants: SDK REST contract unchanged; client keys never see targeting rules (no local
  evaluation); MIT package, no server code; public API unchanged.
- Worktree only; trunk changes only through the PR.
- Commit format: `fix(sdk): <imperative, lower-case>`, body explains why and ends `Fixes #91`; no
  attribution of any kind (no Co-Authored-By, no "Generated with", no credit markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/rust-server/src/cache.rs` | Two-level cache, `context_key`, per-flag bound, unit tests |
| **Modify.** `packages/rust-server/src/client.rs` | `evaluate` keys the cache by the prepared context; `invalidate_cache` doc |
| **Modify.** `packages/rust-server/tests/integration_test.rs` | Per-context caching tests |
| **Modify.** `packages/rust-server/README.md` | Drop the #91 caveat; say what the cache is keyed by |

## Task Order & Rationale

One task: the cache signature change and its only caller must change together to compile.

---

### Task 1: Key the cache by flag and context

**Files:**
- Modify: `packages/rust-server/src/cache.rs`
- Modify: `packages/rust-server/src/client.rs` (`evaluate`, `invalidate_cache`)
- Modify: `packages/rust-server/README.md` (the "Caching caveat" block after the Config example)
- Test: `packages/rust-server/tests/integration_test.rs`

**Interfaces:** `pub(crate) fn context_key(&Context) -> Option<String>`;
`Cache::get_entry(&self, flag: &str, ctx_key: &str)`;
`Cache::set(&self, flag: &str, ctx_key: String, value, configuration, variation)`;
`Cache::invalidate(Option<&str>)` and `Cache::clear()` keep their signatures.

**Out-of-band / invariants:** none out-of-band; request body unchanged.

**Model:** sonnet

- [ ] **Step 1: Write the failing integration tests** (append to `tests/integration_test.rs`)

```rust
#[tokio::test]
async fn different_users_each_get_their_own_cached_answer() {
    let server = MockServer::start().await;
    for (user, enabled) in [("user-a", true), ("user-b", false)] {
        Mock::given(method("POST"))
            .and(path("/api/flags/targeted/evaluate"))
            .and(body_partial_json(json!({ "context": { "user_id": user } })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": enabled })))
            .expect(1)
            .mount(&server)
            .await;
    }

    let client = client(&server);
    let a = || Some(Context::new().with_user_id("user-a"));
    let b = || Some(Context::new().with_user_id("user-b"));

    let first_a = client.evaluate("targeted", a()).await.unwrap();
    let first_b = client.evaluate("targeted", b()).await.unwrap();
    assert!(first_a.value);
    assert!(!first_b.value);
    assert_eq!(first_b.reason, "evaluated");

    let again_a = client.evaluate("targeted", a()).await.unwrap();
    let again_b = client.evaluate("targeted", b()).await.unwrap();
    assert!(again_a.value);
    assert!(!again_b.value);
    assert_eq!(again_a.reason, "cached");
    assert_eq!(again_b.reason, "cached");
}

#[tokio::test]
async fn contexts_differing_in_an_attribute_each_get_their_own_answer() {
    let server = MockServer::start().await;
    for (plan, enabled) in [("pro", true), ("free", false)] {
        Mock::given(method("POST"))
            .and(path("/api/flags/targeted/evaluate"))
            .and(body_partial_json(
                json!({ "context": { "attributes": { "plan": plan } } }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": enabled })))
            .expect(1)
            .mount(&server)
            .await;
    }

    let client = client(&server);
    let ctx = |plan: &str| {
        Some(
            Context::new()
                .with_user_id("user-1")
                .with_attribute("plan", json!(plan)),
        )
    };
    assert!(client.evaluate("targeted", ctx("pro")).await.unwrap().value);
    assert!(!client.evaluate("targeted", ctx("free")).await.unwrap().value);
}

#[tokio::test]
async fn an_identical_context_hits_the_cache_whatever_the_attribute_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": true })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let first = Context::new()
        .with_user_id("user-1")
        .with_attribute("a", json!(1))
        .with_attribute("b", json!({ "x": 1, "y": 2 }));
    let second = Context::new()
        .with_attribute("b", json!({ "y": 2, "x": 1 }))
        .with_attribute("a", json!(1))
        .with_user_id("user-1");
    client.evaluate("test-flag", Some(first)).await.unwrap();
    let cached = client.evaluate("test-flag", Some(second)).await.unwrap();
    assert_eq!(cached.reason, "cached");
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --locked --test integration_test`
Expected: `different_users_each_get_their_own_cached_answer` and
`contexts_differing_in_an_attribute_each_get_their_own_answer` FAIL (the second context is served
the first's cached value; wiremock's `expect(1)` on the second mock reports 0 calls).

- [ ] **Step 3: Implement `src/cache.rs`**

```rust
use crate::types::Context;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Most contexts cached for one flag. Reaching it sweeps that flag's expired entries and, if it
/// is still full, clears them: at most one O(n) pass per this many inserts, and the only cost is
/// cache misses.
pub(crate) const MAX_CONTEXTS_PER_FLAG: usize = 10_000;

#[derive(Clone)]
pub(crate) struct CacheEntry {
    pub value: bool,
    pub configuration: Option<serde_json::Value>,
    pub variation: Option<String>,
    pub expires_at: Instant,
}

/// Thread-safe cache of evaluation results, keyed by flag key and then by evaluation context:
/// the server's answer depends on the context (targeting rules, percentage rollouts), so a
/// result is only reused for the context that produced it (#91).
#[derive(Clone)]
pub(crate) struct Cache {
    entries: Arc<RwLock<HashMap<String, HashMap<String, CacheEntry>>>>,
    ttl: Duration,
}

/// The cache key for a context: its JSON with every object's keys sorted, so two contexts that
/// serialize to the same request body share a key regardless of `HashMap` iteration order (or
/// serde_json's `preserve_order`). `None` (never expected) means "do not cache this call".
pub(crate) fn context_key(context: &Context) -> Option<String> {
    let value = serde_json::to_value(context).ok()?;
    serde_json::to_string(&sorted(value)).ok()
}

fn sorted(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            serde_json::Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted).collect())
        }
        other => other,
    }
}

impl Cache {
    pub fn new(ttl: Duration) -> Self {
        Self { entries: Arc::new(RwLock::new(HashMap::new())), ttl }
    }

    /// The unexpired entry for this flag and context, if any.
    pub fn get_entry(&self, flag_key: &str, context_key: &str) -> Option<CacheEntry> {
        let entries = self.entries.read().ok()?;
        let entry = entries.get(flag_key)?.get(context_key)?;
        if Instant::now() > entry.expires_at {
            return None;
        }
        Some(entry.clone())
    }

    pub fn set(
        &self,
        flag_key: &str,
        context_key: String,
        value: bool,
        configuration: Option<serde_json::Value>,
        variation: Option<String>,
    ) {
        if let Ok(mut entries) = self.entries.write() {
            let contexts = entries.entry(flag_key.to_string()).or_default();
            if contexts.len() >= MAX_CONTEXTS_PER_FLAG && !contexts.contains_key(&context_key) {
                let now = Instant::now();
                contexts.retain(|_, entry| entry.expires_at >= now);
                if contexts.len() >= MAX_CONTEXTS_PER_FLAG {
                    contexts.clear();
                }
            }
            contexts.insert(
                context_key,
                CacheEntry { value, configuration, variation, expires_at: Instant::now() + self.ttl },
            );
        }
    }

    /// Invalidate one flag (for every context) or, with `None`, everything.
    pub fn invalidate(&self, flag_key: Option<&str>) { /* remove(k) / clear(), as today */ }

    pub fn clear(&self) { self.invalidate(None); }
}
```

Unit tests in `#[cfg(test)] mod tests` at the bottom of `src/cache.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_key_ignores_insertion_order_and_separates_values() {
        let a = Context::new().with_attribute("x", json!(1)).with_attribute("y", json!({"p": 1, "q": 2}));
        let b = Context::new().with_attribute("y", json!({"q": 2, "p": 1})).with_attribute("x", json!(1));
        let c = Context::new().with_attribute("x", json!(2)).with_attribute("y", json!({"p": 1, "q": 2}));
        assert_eq!(context_key(&a), context_key(&b));
        assert_ne!(context_key(&a), context_key(&c));
        assert_ne!(context_key(&Context::new().with_user_id("a")), context_key(&Context::new().with_user_id("b")));
    }

    #[test]
    fn invalidating_a_flag_drops_every_context_of_that_flag_only() {
        let cache = Cache::new(Duration::from_secs(60));
        cache.set("f", "c1".into(), true, None, None);
        cache.set("f", "c2".into(), false, None, None);
        cache.set("g", "c1".into(), true, None, None);
        cache.invalidate(Some("f"));
        assert!(cache.get_entry("f", "c1").is_none());
        assert!(cache.get_entry("f", "c2").is_none());
        assert!(cache.get_entry("g", "c1").is_some());
    }

    #[test]
    fn a_full_flag_is_cleared_and_still_takes_the_new_entry() {
        let cache = Cache::new(Duration::from_secs(60));
        for i in 0..MAX_CONTEXTS_PER_FLAG {
            cache.set("f", i.to_string(), true, None, None);
        }
        cache.set("f", "new".into(), false, None, None);
        assert_eq!(cache.get_entry("f", "new").map(|e| e.value), Some(false));
        assert!(cache.get_entry("f", "0").is_none());
        assert_eq!(cache.entries.read().unwrap()["f"].len(), 1);
    }

    #[test]
    fn a_full_flag_keeps_unexpired_entries_after_sweeping_expired_ones() {
        let cache = Cache::new(Duration::ZERO); // every entry expires immediately
        for i in 0..MAX_CONTEXTS_PER_FLAG {
            cache.set("f", i.to_string(), true, None, None);
        }
        std::thread::sleep(Duration::from_millis(2));
        cache.set("f", "new".into(), false, None, None);
        assert_eq!(cache.entries.read().unwrap()["f"].len(), 1);
    }
}
```

(Adapt the second sweep test if needed: the point is that expired entries are swept before a
clear. A cache with zero TTL proves the sweep path; the first test proves the clear path.)

- [ ] **Step 4: Update `FlagClient::evaluate` in `src/client.rs`**

Move the "Prepare context" block (`context.unwrap_or_default()` and the `application_id` fill-in)
above the cache lookup; compute `let ctx_key = context_key(&ctx);`; replace
`self.cache.get_entry(flag_key)` with
`ctx_key.as_deref().and_then(|k| self.cache.get_entry(flag_key, k))`; replace the `set` call with
`if let Some(k) = ctx_key { self.cache.set(flag_key, k, value, configuration.clone(), variation.clone()); }`.
Import `crate::cache::context_key`. Document on `evaluate` that results are cached per flag and
context for `cache_ttl`, and on `invalidate_cache` that `Some(flag)` covers every context.

- [ ] **Step 5: README**

Delete the "Caching caveat" blockquote (four lines starting `> **Caching caveat:**`). Replace it
with one sentence: "Results are cached per flag key and evaluation context for `cache_ttl`; a
different context (another user, other attributes) is evaluated by the server."

- [ ] **Step 6: Run the gates**

Run in `packages/rust-server`: `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`.
Expected: all pass; record per-binary pass counts.

- [ ] **Step 7: Commit**

```bash
git add packages/rust-server
git commit -m "fix(sdk): cache rust sdk evaluations per flag and context" -m "<why>" -m "Fixes #91"
```
