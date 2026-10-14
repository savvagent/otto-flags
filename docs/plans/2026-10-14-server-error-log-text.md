# Server Failure Logging Outside flags-mcp Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Every failure logged above `debug` in `flags-api`, `flags-core`, and `flags-server`
carries a text-free reason (and code/status where available); the error text is logged only at
`debug`.

**Architecture:** Reuse #29's vocabulary in `crates/flags-core/src/error.rs`
(`Error::failure_reason`, `platform_failure_reason`), make `db_failure_reason` public, and add
`ShipError::failure_reason` and a webhook `rejection_reason`. Each site emits its existing level
with the fields, then `tracing::debug!` with the same fields plus `error = %e`, as
`crates/flags-core/src/rollback.rs:1162-1165` does. Control flow and responses are unchanged.

**Tech Stack:** Rust (stable, edition 2021), `tracing`, `tracing-subscriber` (test only), sqlx,
axum, `otto-resource`.

**Ticket:** savvagent/otto-flags#35

**Spec:** `docs/specs/2026-10-14-server-error-log-text-design.md`

## Global Constraints

- Gates (run from the worktree root):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - crates/** changed: `podman build -t otto-flags:check .` (final gate run only)
- Evidence: report pass counts per test binary. `cargo test` without `DATABASE_URL` panics
  ("DATABASE_URL must be set"); that is a missing variable, not a test failure. Skipped tests are
  not a pass. `DATABASE_URL` points only at the local Postgres on 15434. Do not run DB tests from
  another worktree at the same time.
- Invariants that apply: database error text never reaches a tool or HTTP response (responses
  unchanged); platform webhooks stay signature-verified and fail closed (status codes unchanged);
  SDK REST contract stable; no SQL outside `flags-core` and no `query!` macros (no new SQL here);
  no migration.
- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-35-log-error-codes-outside-mcp`);
  trunk changes only through the PR.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Refs #35` (the final squash carries `Fixes #35`); no attribution of any kind (no
  Co-Authored-By, no "Generated with", no credit markers).
- Field names: `code` (wire code, only where a `flags_core::Error` is held), `reason`, `status`
  (`Option<u16>`, logged as the `tracing` optional value). Messages stay as they are today.
- Do not touch `crates/flags-mcp`.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/error.rs` | `db_failure_reason` becomes `pub` with a doc comment |
| **Modify.** `crates/flags-core/src/usage.rs` | `ShipError::failure_reason`; quota fail-open, `reschedule`, shipping loop, final flush log by reason |
| **Modify.** `crates/flags-core/src/notify.rs` | listener connect / LISTEN / recv / payload failures log by reason |
| **Modify.** `crates/flags-api/src/error.rs` | `From<flags_core::Error>` logs code/reason/status at error, text at debug |
| **Modify.** `crates/flags-api/src/platform.rs` | `rejection_reason`; bad-body, rejected, and apply-failure logs by reason |
| **Create.** `crates/flags-api/tests/failure_logs.rs` | log-capture test in its own binary |
| **Modify.** `crates/flags-api/Cargo.toml`, `Cargo.lock` | dev-dependencies `tracing-subscriber`, `sqlx` (workspace) |
| **Modify.** `crates/flags-server/src/main.rs` | shutdown `JoinError`s, sweeps, startup platform check log by reason |
| **Modify.** `crates/flags-server/src/health.rs` | readiness failure logs by reason |

## Task Order & Rationale

Task 1 makes `db_failure_reason` public and adds `ShipError::failure_reason`, which Tasks 2 and 3
consume. Tasks 2 and 3 are independent of each other. Task 4 is the cross-crate grep check.

---

### Task 1: flags-core reasons and logging

**Files:**
- Modify: `crates/flags-core/src/error.rs` (`fn db_failure_reason`, ~line 173)
- Modify: `crates/flags-core/src/usage.rs` (lines ~140-160, ~250-255, ~346-352, ~378-396; tests at ~403)
- Modify: `crates/flags-core/src/notify.rs` (lines ~93-120)

**Interfaces:** produces `pub fn flags_core::error::db_failure_reason(&sqlx::Error) -> &'static str`
and `pub fn ShipError::failure_reason(&self) -> (&'static str, Option<u16>)`.

**Out-of-band / invariants:** none; no SQL text changes.

**Model:** sonnet — mechanical edits with a small new method.

- [ ] **Step 1: Write the failing test** in `usage.rs`'s `mod tests`:

```rust
#[test]
fn ship_errors_name_their_kind_without_the_text() {
    let platform = ShipError::Platform(otto_resource::Error::Status {
        status: 502,
        body: "secret".into(),
    });
    assert_eq!(platform.failure_reason(), ("http_status", Some(502)));
    let db = ShipError::Db(sqlx::Error::Protocol("secret".into()));
    assert_eq!(db.failure_reason(), ("protocol", None));
}
```

- [ ] **Step 2: Run it and see it fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --lib ship_errors_name`
Expected: compile error, no method `failure_reason` on `ShipError`.

- [ ] **Step 3: Implement**
  - `error.rs`: `fn db_failure_reason` → `pub fn db_failure_reason`, doc comment noting it is
    [`Error::failure_reason`] for a bare `sqlx::Error` (as `platform_failure_reason`'s says).
  - `usage.rs`: `impl ShipError { pub fn failure_reason(&self) -> (&'static str, Option<u16>) }`
    with a doc line (text-free, for logs above debug): `Platform(p)` →
    `crate::error::platform_failure_reason(p)`, `Db(e)` → `(crate::error::db_failure_reason(e), None)`.
  - `usage.rs` quota fail-open (~line 140-156): replace the `String` `failure` with
    `(reason, status)`: `Ok(Err(e))` → log `tracing::debug!(org = %org, error = %e, "could not read
    usage from the platform")` and yield `platform_failure_reason(&e)`; `Err(_)` →
    `("timed_out", None)`. The warn becomes
    `tracing::warn!(org = %org, reason, status, "could not read usage from the platform; allowing billable calls for {FAIL_OPEN_FOR:?}")`.
  - `reschedule` (~350): `tracing::warn!(reason = db_failure_reason(&e), "could not record a usage shipping failure")`
    then `tracing::debug!(reason = …, error = %e, same message)`.
  - shipping loop `other =>` arm (~384): `let (reason, status) = other.failure_reason();`
    `warn!(reason, status, retry_in = ?wait, "usage shipping failed")` then
    `debug!(reason, status, error = %other, "usage shipping failed")`. The `Unauthorized` arm is unchanged.
  - final flush (~394): same pattern with `e.failure_reason()`.
  - `notify.rs`: connect failure (~95), LISTEN failure (~103), recv failure (~117): `warn!(reason =
    crate::error::db_failure_reason(&e), "<same message>")` then `debug!(reason = …, error = %e,
    "<same message>")`. Bad payload (~114), a `serde_json::Error`, not a database error: `warn!(reason = "decode", "unreadable flag-change
    notification")` then `debug!(reason = "decode", error = %e, …)`; turn the one-expression match
    arm into a block.

- [ ] **Step 4: Run and see it pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core`
Expected: all pass (report counts per binary). Then `cargo clippy --workspace --all-targets -- -D warnings`
and `cargo fmt --all -- --check` clean.

- [ ] **Step 5: Commit**

`fix(core): log usage and listener failures by reason, text only at debug` — body: database and
platform text (SQL, schema names, response bodies, URLs) was logged at warn; log a text-free
reason and keep the text at debug. Ends `Refs #35`.

---

### Task 2: flags-api logging and its capture test

**Files:**
- Modify: `crates/flags-api/src/error.rs:46-62`
- Modify: `crates/flags-api/src/platform.rs:26-60` (and a `#[cfg(test)] mod tests` at the end)
- Create: `crates/flags-api/tests/failure_logs.rs`
- Modify: `crates/flags-api/Cargo.toml` (`[dev-dependencies]`: `tracing-subscriber.workspace = true`, `sqlx.workspace = true`), `Cargo.lock`

**Interfaces:** consumes `flags_core::Error::failure_reason` (exists).

**Out-of-band / invariants:** SDK REST contract and webhook status codes must not change; the
existing e2e tests in `crates/flags-server/tests/e2e.rs` pin them.

**Model:** sonnet.

- [ ] **Step 1: Write the failing tests**
  - First add the dev-dependencies to `crates/flags-api/Cargo.toml` (`tracing-subscriber.workspace
    = true`, `sqlx.workspace = true`), so the test compiles and fails for the right reason.
  - `crates/flags-api/tests/failure_logs.rs`, modeled on `crates/flags-mcp/tests/failure_logs.rs`
    (copy its `Capture` writer and `capture` helper; module doc: SDK request failures are logged
    by code and reason at error, text only at debug (#35); own binary for the callsite-interest
    caching reason). One test, two cases, each calling `flags_api::ApiError::from(e)`:
    - `Error::Db(sqlx::Error::Protocol("MARKER_DB".into()))` → ERROR line contains
      `code="internal_error" reason="protocol"`; assert `api.status == 503` and
      `!api.message.contains("MARKER")`.
    - `Error::Platform(otto_resource::Error::Status { status: 503, body: "MARKER_PLATFORM".into() })`
      → ERROR line contains `code="platform_unavailable" reason="http_status" status=503`.
    Assert: fields present at ERROR, marker absent from every ERROR line, marker present at DEBUG.
  - `platform.rs` unit test `rejection_reasons_name_each_variant`: `MalformedHeader` →
    `"malformed_header"`, `BadSignature` → `"bad_signature"`, `Stale` → `"stale"`,
    `BadBody("secret".into())` → `"bad_body"`.

- [ ] **Step 2: Run and see them fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-api --test failure_logs` (marker found at ERROR) and
`DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-api --lib rejection_reasons` (no function `rejection_reason`).

- [ ] **Step 3: Implement**
  - `error.rs`: in the `e if e.is_internal()` arm, `let (reason, status) = e.failure_reason();`
    `tracing::error!(code = e.code(), reason, status, "SDK request failed");`
    `tracing::debug!(code = e.code(), reason, error = %e, "SDK request failed");` Response unchanged.
  - `platform.rs`: `fn rejection_reason(e: &WebhookError) -> &'static str`, exhaustive match, with
    a doc line. `BadBody(reason)` arm: `tracing::error!(reason = "bad_body", "platform webhook was
    signed but unreadable")` then `tracing::debug!(reason = "bad_body", error = %detail, …)` (rename
    the binding to `detail`). `Err(e)` arm: `warn!(reason = rejection_reason(&e), "platform webhook
    rejected")` then `debug!(…, error = %e, …)`. Apply failure: `let (reason, status) =
    e.failure_reason();` `error!(code = e.code(), reason, status, event_id = %event.id, "could not
    apply a platform event")` then `debug!(code = e.code(), reason, event_id = %event.id, error =
    %e, …)`. Responses unchanged.
  - Commit the `Cargo.lock` update cargo made in Step 1.

- [ ] **Step 4: Run and see them pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-api -p flags-server`
(flags-server's e2e covers webhook and SDK status codes). Then clippy and fmt as in Global
Constraints.

- [ ] **Step 5: Commit**

`fix(api): log sdk and webhook failures by reason, text only at debug` — body as Task 1's, plus:
the webhook's bad-body text is serde's and can quote the body. Ends `Refs #35`.

---

### Task 3: flags-server logging

**Files:**
- Modify: `crates/flags-server/src/main.rs:104-110, 126-137, 158-162`
- Modify: `crates/flags-server/src/health.rs:64-67`

**Interfaces:** consumes `flags_core::error::{db_failure_reason, platform_failure_reason}` and
`flags_core::Error::failure_reason`.

**Out-of-band / invariants:** `/readyz` status and body unchanged; the boot log markers
`fly.md`/verify rely on ("the otto platform accepted this service's credential") unchanged.

**Model:** sonnet. These paths fire only on runtime failures; there is no test to write first (spec
§11 records the gap). Verify by build, clippy, and the grep in Task 4.

- [ ] **Step 1: Implement**
  - `main.rs` shipper join (~105) and checker join (~109): `let reason = if e.is_panic() {
    "panicked" } else { "cancelled" };` `warn!(reason, "<same message>")` then `debug!(reason, error
    = %e, "<same message>")`.
  - sweeps (~130, ~135): `let (reason, status) = e.failure_reason();` `warn!(code = e.code(),
    reason, status, "<same message>")` then `debug!(code = e.code(), reason, error = %e, …)`.
  - `check_platform` `Err(e)` (~158): `let (reason, status) =
    flags_core::error::platform_failure_reason(&e);` `warn!(platform = %config.platform_url, reason,
    status, "could not reach the otto platform at startup; continuing")` then `debug!(…, error = %e, …)`.
  - `health.rs` (~65): `let reason = flags_core::error::db_failure_reason(&e);` `warn!(reason,
    "readiness check failed")` then `debug!(reason, error = %e, …)`. (`flags-core` is already a
    flags-server dependency.)

- [ ] **Step 2: Run the gates**

`DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server`, clippy,
fmt.

- [ ] **Step 3: Commit**

`fix(server): log shutdown, sweep, and readiness failures by reason, text only at debug` — body as
Task 1's. Ends `Refs #35`.

---

### Task 4: Cross-crate check and full gates

**Model:** run by the orchestrator — commands only, no edits unless a site was missed.

- [ ] **Step 1:** `grep -rnE 'tracing::(error|warn|info)!\([^)]*error = %' crates/` prints nothing.
- [ ] **Step 2:** the span-aware check (spec §4) prints only `crates/flags-core/src/usage.rs`'s
  `reason = %r.reason` line (the platform's rejection reason; out of scope per spec §2):

```sh
find crates -name '*.rs' -print0 | xargs -0 perl -0ne 'while (/tracing::(error|warn|info)!\((.*?)"\s*,?\s*\)/sg) { my $b=$2; $b =~ s/\s+/ /g; print "$ARGV: $b\n" if $b =~ /error = %|reason = %|%reason/ }'
```

- [ ] **Step 3:** If any step finds a site, fix it in the owning crate with the same pattern and
  commit `fix(<scope>): …` ending `Refs #35`; otherwise no commit.
- [ ] **Step 4: Full gate run** on the final head, reporting pass counts per test binary:
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`,
  `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` (prints
  nothing), and `podman build -t otto-flags:check .`.
