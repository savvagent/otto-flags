# Usage Platform Text Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Usage shipping logs a refused event at `error` with `event_id` and a fixed reason (the
platform's reason only at `debug`), and stores a text-free reason and status in
`usage_outbox.last_error` instead of the platform error's text.

**Architecture:** All production changes are in `crates/flags-core/src/usage.rs`: a private
`stored_reason` formats `platform_failure_reason` for the column, both `reschedule` callers use it,
and the refused-event log splits into an `error` line without the platform's text and a `debug`
line with it. `log_text.rs` loses its only exception. A new single-test binary proves the logs and
the column against a mock platform and the local Postgres.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16, axum, tracing.

**Ticket:** savvagent/otto-flags#44

**Spec:** `docs/specs/2026-10-14-usage-platform-text-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-44-usage-platform-text`);
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
- Invariants that apply: no migration (the `last_error` column is unchanged); metering unchanged
  (rows recorded, shipped, deleted, and moved to `usage_outbox_rejected` exactly as before); no
  SQL outside flags-core in production code, runtime `sqlx::query` only; remote text never logged
  above `debug`.
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #44`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/usage.rs` | `stored_reason`, both `reschedule` callers, refused-event logging, unit test |
| **Create.** `crates/flags-server/tests/usage_logs.rs` | One log-capture + DB test against a mock platform |
| **Modify.** `crates/flags-server/tests/log_text.rs` | Drop `ALLOWED` and its check |
| **Modify.** `docs/deploy/fly.md` | One sentence: what `last_error` holds |

## Task Order & Rationale

One task: the production change is a few lines in one module, and its tests only make sense
together.

---

### Task 1: Keep the platform's text out of the error log and the outbox

**Files:**
- Modify: `crates/flags-core/src/usage.rs:314-374` and its `mod tests`
- Create: `crates/flags-server/tests/usage_logs.rs`
- Modify: `crates/flags-server/tests/log_text.rs:33-37, 62-67`
- Modify: `docs/deploy/fly.md:156-160`

**Interfaces:** none public. `stored_reason` and `reschedule` are private to `usage.rs`.

**Out-of-band / invariants:** no migration; metering behavior unchanged; remote text only at
`debug`.

**Model:** sonnet — contained change with fully specified code.

- [x] **Step 1: Write the failing unit test** in `crates/flags-core/src/usage.rs` `mod tests`:

```rust
#[test]
fn the_stored_reason_names_the_failure_without_its_text() {
    let status = otto_resource::Error::Status {
        status: 502,
        body: "secret".into(),
    };
    assert_eq!(stored_reason(&status), "http_status 502");
    assert_eq!(
        stored_reason(&otto_resource::Error::Unauthorized),
        "unauthorized"
    );
    assert_eq!(
        stored_reason(&otto_resource::Error::Decode("secret".into())),
        "decode"
    );
}
```

- [x] **Step 2: Write the failing integration test** `crates/flags-server/tests/usage_logs.rs`
  (a new binary with exactly one test). Model the capture helpers on
  `crates/flags-api/tests/failure_logs.rs` (`Capture` writer) and the mock platform on
  `crates/flags-server/tests/e2e.rs` (`start_platform`, `server`). Contents:

  - Module doc: usage shipping keeps the platform's text out of logs above `debug` and out of
    `usage_outbox.last_error` (#44); its own binary with one test because `tracing` caches
    callsite interest.
  - Markers: `const BODY_MARKER: &str = "body-marker-7f3a";` and
    `const REASON_MARKER: &str = "reason-marker-c91e";`.
  - Mock platform: an axum `Router` with only `POST /internal/usage`, state
    `Arc<Mutex<usize>>` counting calls. Call 1 answers `502 Bad Gateway` with body
    `format!("upstream said {BODY_MARKER}")`. Later calls answer
    `Json(UsageReceipt { accepted: 0, duplicates: 0, rejected })` rejecting every event in the
    `UsageBatch` with `RejectedEvent { event_id, reason: format!("unknown org {REASON_MARKER}") }`
    (`otto_resource::{RejectedEvent, UsageBatch, UsageReceipt}`). Bind `127.0.0.1:0` and
    `tokio::spawn(axum::serve(...))` as `e2e.rs` does.
  - The test, `#[sqlx::test(migrator = "flags_core::MIGRATOR")] async fn
    usage_shipping_keeps_platform_text_out_of_logs_and_the_outbox(pool: PgPool)`:
    1. Start the mock; `let mut config = flags_server::Config::for_test(); config.platform_url =
       url;` `let platform = flags_server::platform_client(&config).unwrap();`
       `let db = otto_tenant::Db::from_pool(pool.clone());`
    2. Insert one row and get its id:
       `sqlx::query_scalar::<_, Uuid>("INSERT INTO usage_outbox (org_id, user_id, tool, billable) VALUES ($1, NULL, 'create_flag', true) RETURNING event_id")`
       bound to `Uuid::new_v4()`, `fetch_one(&pool)`.
    3. Build a `tracing_subscriber::fmt()` subscriber (`with_ansi(false)`,
       `with_max_level(tracing::Level::DEBUG)`, `with_writer(capture.clone())`, `finish()`), and
       hold `let guard = tracing::subscriber::set_default(subscriber);` for steps 4-6
       (`#[sqlx::test]` runs a current-thread runtime, so the thread default holds across awaits).
    4. `let cfg = flags_core::usage::ShipperConfig::default();`
       `let err = flags_core::usage::ship_once(&db, &platform, &cfg).await.unwrap_err();`
       assert `err.failure_reason() == ("http_status", Some(502))`.
       Read `SELECT last_error FROM usage_outbox WHERE event_id = $1` as `Option<String>`;
       assert it equals `Some("http_status 502")`.
    5. `UPDATE usage_outbox SET next_attempt_at = now()` so the row is due again.
    6. `assert_eq!(ship_once(&db, &platform, &cfg).await.unwrap(), 1);` then `drop(guard)`.
    7. Logs: split the captured text into lines. Lines containing ` ERROR `, ` WARN `, or
       ` INFO ` must not contain `BODY_MARKER` or `REASON_MARKER`. Exactly one ` ERROR ` line
       contains `"refused a usage event"`, and it contains the event id and `reason="rejected"`
       (check the exact rendering of a `&str` field in the captured output and assert that). Some
       ` DEBUG ` line contains both the event id and `REASON_MARKER`.
    8. DB: `SELECT reason FROM usage_outbox_rejected WHERE event_id = $1` equals
       `format!("unknown org {REASON_MARKER}")`; `SELECT count(*) FROM usage_outbox` is 0.

- [x] **Step 3: Remove the exception from `crates/flags-server/tests/log_text.rs`:** delete the
  `ALLOWED` constant and its doc comment, delete `let allowed = …;`, and change the condition to
  `if shorthand || TEXT_FIELDS.iter().any(|t| flat.contains(t))`. If `name` becomes used only for
  the message, keep it there. Leave `TEXT_FIELDS`, `TEXT_ARGS`, and the second test as they are.

- [x] **Step 4: Run the tests and see them fail.**
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --lib stored_reason`
  → compile error (`stored_reason` not found).
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test usage_logs --test log_text`
  → `usage_logs` fails on the `last_error` assertion (the error's text is stored), and `log_text`
  fails naming `flags-core/src/usage.rs: … reason = %r.reason …`.

- [x] **Step 5: Implement in `crates/flags-core/src/usage.rs`:**

  - Add, below `reschedule`:

    ```rust
    /// What `usage_outbox.last_error` records for a failed shipment: the
    /// failure's name and HTTP status (`http_status 502`), never its text,
    /// which can carry a platform response body or URL (#44).
    fn stored_reason(e: &otto_resource::Error) -> String {
        match crate::error::platform_failure_reason(e) {
            (reason, Some(status)) => format!("{reason} {status}"),
            (reason, None) => reason.to_string(),
        }
    }
    ```

  - Platform failure: `reschedule(db, &ids, &stored_reason(&e), cfg).await;`
  - Miscount: keep the `msg` and the `tracing::error!("{msg}; keeping every row and retrying")`
    line; then `let err = otto_resource::Error::Decode(msg);`
    `reschedule(db, &ids, &stored_reason(&err), cfg).await; return Err(err.into());`
  - Refused events, replacing the current `tracing::error!`:

    ```rust
    // The platform's reason is its text, not ours: debug only, and kept in
    // usage_outbox_rejected for operators (#44).
    tracing::error!(event_id = %r.event_id, reason = "rejected",
        "the platform refused a usage event; moving it to usage_outbox_rejected");
    tracing::debug!(event_id = %r.event_id, reason = "rejected", platform_reason = %r.reason,
        "the platform refused a usage event; moving it to usage_outbox_rejected");
    ```

  - `reschedule`: rename the parameter `error` to `reason` and bind it directly
    (`.bind(reason)`), dropping `.chars().take(500).collect::<String>()`.

- [x] **Step 6: Run the tests and see them pass** (same two commands as Step 4).

- [x] **Step 7: Docs.** In `docs/deploy/fly.md`, after the usage-backlog SQL block, add:
  "`last_error` names the last failure and its HTTP status (`http_status 502`, `transport`,
  `unauthorized`; a miscounted receipt stores `decode`, and the counts are in the `error` log);
  the failure's text is logged only at `debug`."

- [x] **Step 8: Run every gate** (Global Constraints), recording pass counts per test binary and
  `git rev-parse HEAD`.

- [x] **Step 9: Commit**

  ```
  fix(core): keep platform text out of usage error logs and the outbox

  The platform's per-event rejection reason was logged at error and the
  platform error's full text (response body, URL) was stored in
  usage_outbox.last_error. Both are the platform's text, not ours: the
  refused event now logs a fixed reason at error with its event id (the
  platform's reason at debug and still in usage_outbox_rejected), and
  last_error stores the failure's name and status. log_text.rs loses its
  only exception.

  Fixes #44
  ```
