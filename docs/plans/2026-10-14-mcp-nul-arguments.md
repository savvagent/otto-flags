# MCP NUL Arguments Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** No MCP tool argument containing NUL reaches a statement that fails on it: lookups find
nothing, writes are refused as `invalid_argument`.

**Architecture:** Guards in `flags-core`, each in the function that owns the statement:
`flags::unstorable` (from #60) short-circuits `resolve_app`, `find_policy`, `remove_policy`;
`telemetry::health` normalizes its environment filter with telemetry's own `stored`;
`validate_text` and a new `eval::contains_nul` refuse NUL in written text and JSON.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16, axum, rmcp.

**Ticket:** savvagent/otto-flags#61

**Spec:** `docs/specs/2026-10-14-mcp-nul-arguments-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-61-mcp-nul-arguments`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root. Every DB test run is serialized
  across worktrees with the lock shown (other agents share the Postgres cluster):
  - test: `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .` (run by the orchestrator before the PR)
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434.
- Invariants that apply: no SQL outside flags-core, runtime `sqlx::query` only; tenant isolation
  unchanged (no new statement); request paths keep `begin_live`; SDK-reported text stays untrusted
  (`health` output still goes through `inert`); every flag change versioned (a refused write
  changes nothing).
- Out-of-band: none. No migration.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #61`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/flags.rs` | `unstorable` doc generalized; `validate_text` refuses NUL; configuration check in `create_flag`/`update_flag` |
| **Modify.** `crates/flags-core/src/eval.rs` | `contains_nul`; `validate_variations` and `validate_env` refuse NUL; unit tests |
| **Modify.** `crates/flags-core/src/apps.rs` | `resolve_app` skips the query for an unstorable name |
| **Modify.** `crates/flags-core/src/rollback.rs` | `find_policy`/`remove_policy` skip the statement for an unstorable environment |
| **Modify.** `crates/flags-core/src/telemetry.rs` | `MAX_ENVIRONMENT`; `health` normalizes its environment with `stored` |
| **Modify.** `crates/flags-core/tests/db.rs` | DB test across lookups and writes |
| **Modify.** `crates/flags-server/tests/e2e.rs` | e2e: lookup and write tools with NUL arguments |

## Task Order & Rationale

One task: the guards are a few lines each and share one test pair; splitting them would only
split the tests.

---

### Task 1: Answer NUL in MCP tool arguments with not-found or invalid_argument

**Model:** sonnet

**Files:**
- Modify: `crates/flags-core/src/flags.rs`, `crates/flags-core/src/eval.rs`,
  `crates/flags-core/src/apps.rs`, `crates/flags-core/src/rollback.rs`,
  `crates/flags-core/src/telemetry.rs`
- Test: `crates/flags-core/src/eval.rs` (unit), `crates/flags-core/tests/db.rs`,
  `crates/flags-server/tests/e2e.rs`

- [ ] **Step 1: Unit tests in `eval.rs`** (`mod tests`):
  `contains_nul_finds_nul_anywhere`: true for `json!("a\u{0}")`, `json!({"k\u{0}": 1})`,
  `json!({"a": [1, {"b": "\u{0}"}]})`; false for `json!({"a": ["b", 1, null, true]})`.
  `validation_refuses_nul`: `validate_variations(&json!({"on": {"configuration": {"x": "\u{0}"}}}))`
  is `Err` containing `"NUL"`; `validate_env` with a rule `{attribute: "plan", operator: Equals,
  values: [json!("a\u{0}")]}` is `Err` containing `"rule 0"` and `"NUL"`; the same with
  `attribute: "pl\u{0}an"` (operator `Exists`, no values) and with `description: Some("d\u{0}")`
  is `Err`.
  Run: `cargo test -p flags-core --lib -- contains_nul validation_refuses_nul` (no database).
  Expected: FAIL (does not compile: `contains_nul` is missing).

- [ ] **Step 2: DB test** in `crates/flags-core/tests/db.rs`, after `a_key_with_nul_finds_no_flag`:
  `arguments_with_nul_find_nothing_or_are_refused` (`#[sqlx::test(migrator = "flags_core::MIGRATOR")]`).
  One org, `tx = db.begin(org)`, app `web` (default environments, i.e. `production`), flag `f`.
  Assert, all in the same transaction:
  - `tx.resolve_app("web\0")` and `tx.resolve_app(" \0")` → `Error::AppNotFound { .. }`.
  - `tx.create_flag(&app, NewFlag { key: "g", name: Some("n\0") .. })` → `Error::Invalid(_)`;
    likewise `description: Some("d\0")`, `reason` arg `Some("r\0")`,
    `variations: Some(json!({"on": {"description": "\u{0}"}}))`,
    `configuration: Some(json!({"k": "\u{0}"}))`.
  - `tx.update_flag(&app, "f", FlagPatch { configuration: Some(json!(["\u{0}"])) .. }, meta(actor))`
    → `Error::Invalid(_)`; `update_flag` with `meta` whose `reason: Some("r\0")` and a valid
    `name` patch → `Error::Invalid(_)`.
  - `tx.set_environment(&app, "f", "production", EnvPatch { rules: Some(vec![Rule { attribute:
    "plan".into(), operator: Operator::Equals, values: vec![json!("a\u{0}")], enabled: None,
    variation: None, description: None }]), .. }, meta(actor))` → `Error::Invalid(_)`.
  - `tx.find_policy(flag.id, "production\0")` → `Ok(None)`;
    `tx.remove_policy(&flag, "production\0", actor)` → `Ok(false)`.
  - Record evaluations through `telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server,
    …)` for flag `f`: 2 with `environment: "production\0"`, 3 with `"production"` (enabled true,
    variation None, at now). `telemetry::health(&mut tx, flag.id, Some("production\0"), 24)` →
    `Ok`, `evaluations.enabled == 2`, and `h.environment` contains no `'\0'`;
    `health(.., Some("production"), ..)` → `evaluations.enabled == 3`.
  - The flag is unchanged (`tx.find_flag(app.id, "f")` version 1) and `tx.commit()` succeeds
    (no statement failed in the transaction).
  Run: `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db arguments_with_nul`
  Expected: FAIL (a database error, e.g. 22021/22P05, or a non-`Invalid` result).

- [ ] **Step 3: e2e test** in `crates/flags-server/tests/e2e.rs`, after
  `evaluating_a_key_with_nul_is_not_found`: `mcp_arguments_with_nul_are_not_internal_errors`.
  Use `start_platform`, `server`, and `org_with_flag(&s, &platform, Uuid::new_v4(), "checkout")`
  (app `web`). Then:
  - Lookups: `get_flag {"app": "web\u0000", "key": "checkout"}` → `Err`, `err["data"]["code"] ==
    "app_not_found"`; `remove_rollback_policy {"app": "web", "key": "checkout", "environment":
    "production\u0000"}` → `Ok`, `r["removed"] == false`; `propose_rollback {"app": "web", "key":
    "checkout", "environment": "production\u0000"}` → `Err`, code `"invalid_argument"`; `flag_health {"app": "web", "key":
    "checkout", "environment": "production\u0000"}` → `Ok`.
  - Writes: `create_flag {"app": "web", "key": "other", "reason": "r\u0000"}`,
    `update_flag {"app": "web", "key": "checkout", "configuration": {"k": "\u0000"}}`,
    `set_flag_environment {"app": "web", "key": "checkout", "environment": "production", "rules":
    [{"attribute": "plan", "operator": "equals", "values": ["a\u0000"]}]}` → each `Err` with
    `err["data"]["code"] == "invalid_argument"`.
  Check the error shape against the existing `evaluating_a_key_with_nul_is_not_found` (it reads
  `err["data"]["code"]`). If a tool's scope needs differ from `org_with_flag`'s token, read that
  helper first.
  Run: `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e mcp_arguments_with_nul`
  Expected: FAIL (`internal_error`).

- [ ] **Step 4: Implement.**
  - `flags.rs`: generalize `unstorable`'s doc ("text no stored value can equal: Postgres `text`
    cannot hold NUL, so such text names nothing stored, and binding it would fail the statement
    (and with it the caller's transaction) instead of finding nothing"); rename its parameter
    `key` → `s`. In `validate_text`, before the length arm: `Some(s) if s.contains('\0') =>
    Err(Error::Invalid(format!("{what} must not contain NUL (U+0000)")))`. In `create_flag` and in
    `update_flag` (inside `if let Some(c) = patch.configuration`), refuse
    `eval::contains_nul(&c)` with `Error::Invalid("configuration must not contain NUL (U+0000)")`.
  - `eval.rs`: `pub(crate) fn contains_nul(v: &Value) -> bool` (match `String` → `contains('\0')`,
    `Array` → any, `Object` → any key or value, else false), with a doc comment: Postgres `jsonb`
    refuses `\u0000`, so a write carrying it would fail its statement. `validate_variations`:
    after the null check, `if contains_nul(v) { return Err("variations must not contain NUL
    (U+0000)".into()) }`. `validate_env`: in the per-rule loop, before the attribute length check,
    refuse `rule.attribute.contains('\0') || rule.description.as_deref().is_some_and(|d|
    d.contains('\0')) || rule.values.iter().any(contains_nul)` with
    `format!("rule {i}: must not contain NUL (U+0000)")`. This is the one place for the rule check:
    `validate_all_envs` reuses `validate_env` over stored environments, which cannot hold NUL, so
    it is unaffected.
  - `apps.rs` `resolve_app` (keep the `Option<FlagApp>` annotation on `found`): `let found = if crate::flags::unstorable(wanted) { None } else {
    <existing query> };` — the not-found branch below is unchanged.
  - `rollback.rs`: `find_policy`: `if crate::flags::unstorable(environment) { return Ok(None); }`;
    `remove_policy`: `if crate::flags::unstorable(environment) { return Ok(false); }`, each with a
    one-line comment (no stored policy environment can contain NUL).
  - `telemetry.rs`: `const MAX_ENVIRONMENT: usize = 64;` (doc: stored environments are capped at
    this), used in place of the literal `64` at the two `stored(…, 64)` environment sites in
    `record_evaluations`/`record_errors`. In `health`, first line:
    `let environment = environment.map(|e| stored(e, MAX_ENVIRONMENT));` with a comment: the
    environment as telemetry stores it, so it finds what an SDK reported under that name and never
    binds NUL. Bind `environment.as_deref()` in the three queries and return it in
    `Health::environment`.

- [ ] **Step 5: Run the targeted tests, then all gates.**
  `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  — all pass, report counts per binary; `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo fmt --all -- --check`; the migrations diff prints nothing.

- [ ] **Step 6: Commit.**
  `fix(core): answer nul in mcp tool arguments without a failed statement`, body: why (an
  argument with NUL failed its statement, so agents got `internal_error` and the server logged a
  failure; lookups now find nothing, writes refuse it as invalid input), ending `Fixes #61`.
