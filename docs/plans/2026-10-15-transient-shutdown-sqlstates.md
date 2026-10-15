# Transient shutdown and startup SQLSTATEs Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `Error::is_transient` returns `true` for Postgres `57P01`, `57P02`, and `57P03`, with the
reasoning in `TRANSIENT_SQLSTATES`' doc comment, and `retriable` unchanged.

**Architecture:** Three codes added to the private `TRANSIENT_SQLSTATES` list in
`crates/flags-core/src/error.rs`; doc comments updated there and in one line of the
`crates/flags-core/src/rollback.rs` module doc. No new types, no SQL, no migration.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8.6.

**Ticket:** savvagent/otto-flags#56

**Spec:** `docs/specs/2026-10-15-transient-shutdown-sqlstates-design.md`

## Global Constraints

- Worktree only:
  `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-56-transient-shutdown-sqlstates`; `main`
  changes only through the PR.
- Every cargo command runs with `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp` from the worktree root
  (`mkdir -p target/tmp` first: the host's `/tmp` tmpfs is nearly full and doc-test linking
  crashes there).
- Gates:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15442/otto_flags cargo test --workspace`
    (this run's dedicated local Postgres on 15442, never 15434 or a Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check` (run `cargo fmt --all` first)
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; once, before the PR)
- Evidence: pass counts per test binary. `cargo test` without `DATABASE_URL` panics ("DATABASE_URL
  must be set"): a missing variable, not a test failure. Skipped or ignored tests are not a pass.
- Invariants that apply: database error text never reaches a tool response (only codes are
  compared); the small shared database (no new fan-out); `retriable` and the MCP contract
  unchanged; no migration.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #56`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/error.rs` | `TRANSIENT_SQLSTATES` + its doc, `is_transient` doc, unit tests |
| **Modify.** `crates/flags-core/src/rollback.rs` | module doc's list of transient failures (doc only) |

## Task Order & Rationale

One task: the change is a constant, its doc, and its test.

---

### Task 1: Class 57P01, 57P02, 57P03 as transient

**Files:**
- Modify: `crates/flags-core/src/error.rs` (`is_transient` doc; `TRANSIENT_SQLSTATES` and its doc;
  tests `only_contention_cancellation_and_connection_failures_are_transient` and
  `retriable_keeps_its_meaning_for_mcp_callers`)
- Modify: `crates/flags-core/src/rollback.rs` (module doc, the paragraph starting "A flag whose
  check fails never advances")

**Interfaces:** none new.

**Out-of-band / invariants:** none out-of-band. `retriable` must not be edited.

**Model:** haiku — fully specified edit.

- [ ] **Step 1: Write the failing test**

In `only_contention_cancellation_and_connection_failures_are_transient`, after
`Error::Db(db("57014")),` in the transient list, add:

```rust
            // admin_shutdown (a restart or terminated backend), crash_shutdown,
            // cannot_connect_now (starting up or in recovery)
            Error::Db(db("57P01")),
            Error::Db(db("57P02")),
            Error::Db(db("57P03")),
            Error::Tenant(otto_tenant::Error::Db(db("57P01"))),
```

and in the permanent list, after `Error::Db(db("42501")),`:

```rust
            // database_dropped, operator_intervention: not self-healing
            Error::Db(db("57P04")),
            Error::Db(db("57000")),
```

The inline comment above is the only comment change in the permanent list.
In `retriable_keeps_its_meaning_for_mcp_callers`, add:

```rust
        assert!(Error::Db(db("57P01")).retriable());
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp cargo test -p flags-core --lib error::tests`
Expected: FAIL in `only_contention_cancellation_and_connection_failures_are_transient` on
`Db(Database(FakeDbError("57P01")))`.

- [ ] **Step 3: Implement**

Replace the `TRANSIENT_SQLSTATES` doc comment and constant with:

```rust
/// SQLSTATEs that a later attempt can avoid (#56):
///
/// - contention and cancellation: serialization_failure `40001`,
///   deadlock_detected `40P01`, lock_not_available `55P03` (`lock_timeout`),
///   query_canceled `57014` (`statement_timeout`);
/// - the server, not the statement, going away and coming back:
///   admin_shutdown `57P01` (a restart or failover, which is what a Fly
///   Postgres restart looks like, or a terminated backend), crash_shutdown
///   `57P02` (another backend crashed and every session was reset), and
///   cannot_connect_now `57P03` (starting up or in recovery). Each clears in
///   seconds, and a later attempt gets a fresh connection; classing them
///   permanent would cool a flag's auto-rollback down for an hour after a
///   routine restart. sqlx's pool already retries `57P03` while connecting.
///
/// Not here: database_dropped `57P04` and other `57xxx`
/// (operator_intervention), which retrying cannot be assumed to fix, and
/// the `08xxx` connection exceptions: sqlx reports a dropped connection as
/// [`sqlx::Error::Io`] (transient), and one Postgres sends, such as
/// protocol_violation `08P01`, is not self-healing.
const TRANSIENT_SQLSTATES: &[&str] = &[
    "40001", "40P01", "55P03", "57014", "57P01", "57P02", "57P03",
];
```

In `is_transient`'s doc comment, change the first sentence to:

```rust
    /// Whether this failure is the kind that passes on its own: contention
    /// (serialization failure, deadlock, a lock or statement timeout), a
    /// connection or pool problem, the database shutting down, restarting, or
    /// still starting up, or the platform being unreachable. Narrower
```

(the rest of that comment unchanged; keep line width ≤ 80 like the surrounding comments).

In `crates/flags-core/src/rollback.rs` module doc, change

```
//! a pool timeout, a serialization failure or deadlock, a lock or statement
//! timeout, the platform unreachable) counts as a check, so the next hint or
```

to

```
//! a pool timeout, a serialization failure or deadlock, a lock or statement
//! timeout, a database restart, the platform unreachable) counts as a check,
//! so the next hint or
```

and reflow the rest of that paragraph so no line exceeds the surrounding width. Change no code
in `rollback.rs`.

Do not touch `retriable`.

- [ ] **Step 4: Run tests to verify they pass, then all gates**

Run: `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp cargo test -p flags-core --lib error::tests`
Expected: all `error::tests` pass.

Then: `cargo fmt --all`, then the full gate list from Global Constraints (test, lint, format,
extra, and `podman build -t otto-flags:check .`). Report pass counts per test binary.

- [ ] **Step 5: Commit**

```bash
git add crates/flags-core/src/error.rs crates/flags-core/src/rollback.rs
git commit -F- <<'EOF'
fix(core): treat postgres shutdown and startup sqlstates as transient

admin_shutdown (57P01), crash_shutdown (57P02) and cannot_connect_now
(57P03) are what a Postgres restart or failover leaves behind, and clear
in seconds. Classing them permanent made the auto-rollback checker cool a
flag down for an hour after a routine Fly Postgres restart. retriable,
what an MCP caller sees, is unchanged.

Fixes #56
EOF
```

After the PR opens, the shipped rule (CLAUDE.md > Design docs) adds the final commit: the spec's
`**Status:**` becomes `IMPLEMENTED (#<PR>)` and these boxes are ticked.
