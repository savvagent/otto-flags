# Evaluate NUL Key Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** A flag key containing NUL is an unknown key wherever a flag is looked up by key (SDK
evaluate, MCP tools, flag writers, telemetry), answered without issuing a query that would fail.

**Architecture:** One `pub(crate) fn unstorable(key) -> bool` in `flags_core::flags`, checked
before the two by-key lookups (`find_flag`, `lock`) and used by `telemetry::flag_ids` in place of
its inline check.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16, axum, rmcp.

**Ticket:** savvagent/otto-flags#45

**Spec:** `docs/specs/2026-10-14-evaluate-nul-key-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-45-evaluate-nul-key`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434. Only one DB test run
  at a time against that Postgres.
- Invariants that apply: the SDK REST contract is stable (a 503 becomes the documented 404, nothing
  else changes); no SQL outside flags-core, runtime `sqlx::query` only; tenant isolation unchanged
  (no new statement); request paths keep `begin_live`.
- Out-of-band: none. No migration.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #45`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/flags.rs` | `unstorable`; guards in `find_flag` and `lock` |
| **Modify.** `crates/flags-core/src/telemetry.rs` | `flag_ids` uses `unstorable` |
| **Modify.** `crates/flags-core/tests/db.rs` | DB test: `find_flag` and a writer with a NUL key |
| **Modify.** `crates/flags-server/tests/e2e.rs` | e2e: both evaluate routes and MCP `get_flag` with a NUL key |

## Task Order & Rationale

One task: one predicate and its three uses; the tests only make sense together.

---

### Task 1: Treat a flag key containing NUL as unknown

**Model:** sonnet

**Files:**
- Modify: `crates/flags-core/src/flags.rs`
- Modify: `crates/flags-core/src/telemetry.rs`
- Modify: `crates/flags-core/tests/db.rs`
- Modify: `crates/flags-server/tests/e2e.rs`

- [x] **Step 1: Write the failing tests.**
  - `crates/flags-core/tests/db.rs`, after `telemetry_rolls_up_and_feeds_health`'s neighbours (any
    place among the flag tests), `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`
    `a_key_with_nul_finds_no_flag`: `db(pool).await`, `OrgId::new()`, `UserId::new()`, `db.begin(org)`,
    `tx.create_app("web", &[], actor)`, flag `f` via `create_flag`. Assert
    `tx.find_flag(app.id, "f\0").await.unwrap().is_none()`, and that
    `tx.update_flag(&app, "f\0", FlagPatch { name: Some("x".into()), ..Default::default() }, meta(actor)).await`
    is `Err(Error::FlagNotFound { .. })` (match with `matches!`). Then `tx.commit()` succeeds (the
    transaction was not aborted by a failed statement).
  - `crates/flags-server/tests/e2e.rs`, after `telemetry_with_nul_text_is_accepted`, with a doc
    comment ("NUL is valid in a path but not in Postgres text; it used to fail the lookup with a
    server error instead of not-found (#45).") and `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`
    `evaluating_a_key_with_nul_is_not_found`: `start_platform`, `server`, `let (t, _, _, client_key) =
    org_with_flag(&s, &platform, Uuid::new_v4(), "checkout").await;`. For each path in
    `["/api/flags/checkout%00/evaluate", "/api/evaluate/checkout%00"]`:
    `sdk(&s.app, "POST", path, &client_key, None)` → `StatusCode::NOT_FOUND` and
    `r["error"] == "flag_not_found"`, and `!r["error_description"].as_str().unwrap().contains('\0')`. Then `/api/flags/checkout/evaluate` → `StatusCode::OK`. Then
    `tool(&s.app, &t, "get_flag", json!({"app": "web", "key": "checkout\u{0}"}))` is `Err(err)` with
    `err["data"]["code"] == "flag_not_found"`.
- [x] **Step 2: Run, see them fail:**
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db a_key_with_nul_finds_no_flag`
  and
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e evaluating_a_key_with_nul_is_not_found`
  — both fail (a database error; a `503 temporarily_unavailable` on the SDK routes and
  `internal_error` over MCP, not not-found).
- [x] **Step 3: Implement** in `crates/flags-core/src/flags.rs`:
  - Near `validate_key`, add
    ```rust
    /// True for a key no stored flag can have: Postgres `text` cannot hold NUL,
    /// so such a key names no flag, and binding it would fail the statement
    /// (and with it the caller's transaction) instead of finding nothing.
    pub(crate) fn unstorable(key: &str) -> bool {
        key.contains('\0')
    }
    ```
  - `find_flag`: first line `if unstorable(key) { return Ok(None); }`.
  - `lock`: before the query, `if unstorable(key) { return Err(Error::FlagNotFound { app: app.name.clone(), key: key.trim().to_string() }); }`.
  - `crates/flags-core/src/telemetry.rs`, `flag_ids`: replace `keys.retain(|k| !k.contains('\0'));`
    with `keys.retain(|k| !crate::flags::unstorable(k));`, and shorten its comment to point at `unstorable`.
- [x] **Step 4: Run, see them pass:** the Step 2 commands, then all gates (Global Constraints),
  recording pass counts per test binary.
- [x] **Step 5: Commit:** `fix(core): answer not-found for a flag key containing nul`, body saying
  why (a public `sdk_` key could turn an unknown-key lookup into a server error and a misleading
  failure log; the MCP tools and writers shared the lookup) and ending `Fixes #45`. No attribution.
