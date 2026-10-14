# Telemetry NUL Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** SDK telemetry batches whose text fields contain NUL are recorded (NUL replaced by
U+FFFD) instead of failing the whole batch.

**Architecture:** `flags_core::telemetry`'s `truncate` helper becomes `stored`, which also replaces
NUL with U+FFFD, so every stored text field is cleaned where it is already capped. `flag_ids` stops
binding keys that contain NUL (no flag key can), so such events count as unknown.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16, axum.

**Ticket:** savvagent/otto-flags#38

**Spec:** `docs/specs/2026-10-14-telemetry-nul-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-38-telemetry-nul`); `main`
  changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434. Only one DB test run
  at a time against that Postgres.
- Invariants that apply: SDK-reported text is untrusted (bound as parameters, never interpolated);
  the SDK REST contract is stable (a failing request now succeeds; nothing else changes); no SQL
  outside flags-core, runtime `sqlx::query` only; no per-row query fan-out; truncation caps
  unchanged; tenant isolation unchanged (no new statement).
- Out-of-band: none. No migration.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #38`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/telemetry.rs` | `truncate` -> `stored` (cap + NUL replacement), `flag_ids` skips NUL keys, unit test |
| **Modify.** `crates/flags-core/tests/db.rs` | DB test: error and evaluation batches with NUL |
| **Modify.** `crates/flags-server/tests/e2e.rs` | Endpoint test: both telemetry endpoints answer 200 with NUL in their text |

## Task Order & Rationale

One task: the change is two small edits in one module, and the tests that prove it only make sense
together.

---

### Task 1: Replace NUL in SDK telemetry text

**Model:** sonnet

**Files:**
- Modify: `crates/flags-core/src/telemetry.rs`
- Modify: `crates/flags-core/tests/db.rs`
- Modify: `crates/flags-server/tests/e2e.rs`

- [ ] **Step 1: Write the DB test** `telemetry_text_with_nul_is_stored_replaced` in
  `crates/flags-core/tests/db.rs`, beside `error_batch_columns_keep_their_own_values`, with
  `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`. Setup as the neighbouring tests: `db(pool)`,
  `OrgId::new()`, `UserId::new()`, `db.begin(org)`, `tx.create_app("web", &[], actor)`, one flag
  `f` via `create_flag`. Then:
  - Record an error batch (`KeyKind::Server`) of two events: (a) flag `f`, `environment:
    Some("prod\0x")`, `error_type: "Type\0Error"`, `error_message: "bad\0message"`, `stack_trace:
    Some("at\0main")`; (b) flag key `"f\0"`, otherwise plain values. Assert the receipt is
    `Receipt { accepted: 1, unknown_flags: 1 }`, `recorded.flags == vec![f]`, and the single
    `flag_errors` row (select `environment, error_type, error_message, stack_trace`) equals
    `("prod\u{FFFD}x", "Type\u{FFFD}Error", "bad\u{FFFD}message", "at\u{FFFD}main")`, and that
    `flag_errors` has exactly 1 row.
  - Record an evaluation batch (`KeyKind::Server`) of three events, all with one shared `at` (a
    single `let now = Utc::now();`, so both land in one hourly bucket): two with flag `f`, `enabled:
    true`, `environment: "stag\0ing"`, `variation: Some("v\0one")`; one with flag key `"f\0"`.
    Assert the receipt is `Receipt { accepted: 2, unknown_flags: 1 }` and `flag_eval_hourly` has
    exactly one row whose `(environment, variation, count)` is
    `("stag\u{FFFD}ing", "v\u{FFFD}one", 2)`.
  - Commit the transaction at the end (`tx.commit().await.unwrap()`), as neighbours do.
- [ ] **Step 2: Write the unit test** `nul_is_replaced_and_text_capped` in `telemetry.rs`'s
  `mod tests`: `stored("a\0b\0", 10) == "a\u{FFFD}b\u{FFFD}"`, `stored("\0\0\0", 2) ==
  "\u{FFFD}\u{FFFD}"`, `stored("abc", 2) == "ab"`. (It will not compile until Step 4; that is its
  failing state.)
- [ ] **Step 3: Run the DB test and see it fail** with the Postgres `0x00` error:
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db telemetry_text_with_nul`
  (Step 2's unit test is in the library, not this binary, so it does not block this run.)
- [ ] **Step 4: Implement** in `crates/flags-core/src/telemetry.rs`:
  - Rename `truncate` to `stored` and make it
    `s.chars().take(max).map(|c| if c == '\0' { '\u{FFFD}' } else { c }).collect()`, with a doc
    comment: Postgres `text` cannot hold NUL, which is valid in JSON and Rust strings, so one NUL
    would fail the whole batch; it is replaced (not dropped) so a malformed value never merges into
    a real environment or variation; #38. Update all six call sites.
  - In `flag_ids`, before binding, drop keys containing `'\0'`
    (`keys.retain(|k| !k.contains('\0'))`; the parameter becomes `mut keys`), with a comment: no flag key can contain NUL
    (`flags::validate_key`), so such a key is unknown like any other, and binding it would fail the
    batch. The callers' `ids.get(&e.flag_key)` miss already counts it as unknown.
- [ ] **Step 5: Run the DB test and the unit test and see them pass:**
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core telemetry`
  (and the `--test db telemetry_text_with_nul` filter from Step 3).
- [ ] **Step 6: Write the endpoint test** `telemetry_with_nul_text_is_accepted` in
  `crates/flags-server/tests/e2e.rs`, after `report`'s helpers: `let (url, platform) =
  start_platform().await; let s = server(pool, &url).await;` then `org_with_flag(&s, &platform,
  Uuid::new_v4(), "checkout")` for the server key. POST `/api/telemetry/errors` via `sdk(...)` with
  one error whose `error_type`, `error_message`, `stack_trace`, and `environment` contain `"\u0000"`
  (serde_json's `json!` with a Rust `"\0"` in the string produces it), plus one with `flag_key:
  "checkout\u0000"`; assert `StatusCode::OK` and body `{"accepted": 1, "unknown_flags": 1}`. Same
  for `/api/telemetry/evaluations` with one event whose `environment` and `variation` contain NUL
  plus one with `flag_key: "checkout\u0000"` (field names as
  in `report`: `flag_key`, `result`, `environment`, `variation`, `timestamp`); assert OK and
  `{"accepted": 1, "unknown_flags": 1}`. Run it:
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e telemetry_with_nul`
  To confirm it tests the fix, temporarily revert `stored`'s replacement with `git stash`-free means
  (e.g. comment out the `.map`), see the test fail with a non-200, and restore it; record both runs.
- [ ] **Step 7: Run every gate** (Global Constraints) and report pass counts per test binary.
- [ ] **Step 8: Commit** the three files with exactly this message (no trailers of any kind):

  ```
  fix(core): replace nul in sdk telemetry text instead of failing the batch

  Postgres text cannot hold NUL, which is valid in JSON and Rust strings,
  so one event with a NUL in any reported field failed the whole telemetry
  batch and the SDK got an error instead of its receipt. Stored text now
  has NUL replaced by U+FFFD as it is capped, and a flag key containing NUL
  is counted as unknown instead of being bound into the key lookup.

  Fixes #38
  ```

The spec's `**Status:**` line and this plan's boxes are updated in the PR's final commit by the
orchestrator.
