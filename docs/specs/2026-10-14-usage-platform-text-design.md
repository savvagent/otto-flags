# Usage shipping: no platform text above debug or in the outbox

**Status:** APPROVED
**Issue:** #44

## 1. Brief

From #44 (found reviewing #42 / #35). Both sites are in `crates/flags-core/src/usage.rs`:

> - **The platform's rejection reason is logged at `error`.** When the platform refuses a usage
>   event, `reason = %r.reason` is written at `error`. It is the platform's free text, and the only
>   remaining text field above `debug`; `crates/flags-server/tests/log_text.rs` names it as an
>   exception. It is useful to operators, but it is the platform's text, not ours.
> - **The outbox stores the platform error's full text.** `reschedule(db, &ids, &e.to_string(),
>   cfg)` writes it to `usage_outbox.last_error`, and that text can include a response body or URL.
>   Nothing in `crates/` reads the column back. `rollback_policies.last_error_code` stores a code
>   instead.
>
> **Acceptance criteria**
> - [ ] A refused usage event is logged at `error` with `event_id` and a fixed reason. The
>   platform's reason moves to `debug` and stays in `usage_outbox_rejected.reason`.
> - [ ] `usage_outbox.last_error` stores the text-free reason and status, not the error text. The
>   column itself does not change, so no migration is needed.
> - [ ] The `usage.rs` exception is removed from `log_text.rs`'s `ALLOWED`.

## 2. Premise corrections

- `reschedule` has two callers, not one: the platform-failure path (`usage.rs:317`, the error's
  `to_string()`) and the miscount path (`usage.rs:328`, our own sentence "the platform accounted
  for N of M usage events"). The miscount text is ours and carries no platform text, but the rule
  for the column is uniform: both callers store the reason of the error `ship_once` returns.
- `ALLOWED` has exactly one entry (the `usage.rs` one). With it gone the list and the code that
  consults it are dead, so both are removed rather than left as an empty list (YAGNI); a future
  exception would re-add them with its justification.

## 3. Assumptions

- **Fixed reason name:** the refused-event `error` line logs `reason = "rejected"` with
  `event_id`. The `debug` line repeats both and adds `platform_reason = %r.reason`, a field name
  that says whose text it is.
- **Stored form of `last_error`:** `<reason>` or `<reason> <status>` (e.g. `http_status 502`,
  `transport`, `unauthorized`, `decode`), from `flags_core::error::platform_failure_reason`, the
  vocabulary the shipping loop already logs (#42). One private helper,
  `fn stored_reason(e: &otto_resource::Error) -> String`, formats it and is unit-tested.
- **Miscount stores `decode`:** `ship_once` returns `otto_resource::Error::Decode(msg)` for a
  miscount and the loop already logs it as `decode`, so the column says the same. The `error` log
  line with the counts is unchanged (our own text).
- **The 500-character cap goes:** the stored value is a short static name plus a `u16`.
- **Existing rows** may still hold old text until their next failure (overwritten) or success
  (deleted); the backlog drains within seconds in normal operation. No data fix (that would be a
  production write).
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

The platform's free text no longer appears above `debug` in logs, nor in `usage_outbox`.

- A refused usage event logs one `ERROR` line with `event_id` and `reason="rejected"` and no
  platform text, and one `DEBUG` line carrying the platform's reason; `usage_outbox_rejected.reason`
  still holds the platform's reason verbatim.
- After a failed shipment (e.g. HTTP 502 with a body), `usage_outbox.last_error` is
  `http_status 502` and contains nothing from the body.
- `log_text.rs` has no exceptions and passes.
- No migration, no wire change, no new dependency.

## 5. Scope

**In:** `crates/flags-core/src/usage.rs`, `crates/flags-server/tests/log_text.rs`, a new test
binary `crates/flags-server/tests/usage_logs.rs`, and one line in `docs/deploy/fly.md` (the
usage-backlog query) saying what `last_error` holds.

**Out:** #43 (sqlstate/transport sub-kinds); the `usage_outbox_rejected.reason` column (kept, per
the AC); any change to what operators see at `warn` from the shipping loop; existing rows' text.

## 6. Architecture

`crates/flags-core/src/usage.rs`:

- `ship_once`, platform failure (`:314-319`): `reschedule(db, &ids, &stored_reason(&e), cfg)`.
- `ship_once`, miscount (`:321-330`): build the `otto_resource::Error::Decode(msg)` first, then
  `reschedule(db, &ids, &stored_reason(&err), cfg)` and return it. The `error!` line is unchanged.
- Refused events (`:333-346`):
  ```rust
  tracing::error!(event_id = %r.event_id, reason = "rejected",
      "the platform refused a usage event; moving it to usage_outbox_rejected");
  tracing::debug!(event_id = %r.event_id, reason = "rejected", platform_reason = %r.reason,
      "the platform refused a usage event; moving it to usage_outbox_rejected");
  ```
  The `INSERT … usage_outbox_rejected` still binds `&r.reason`.
- `reschedule(db, ids, reason: &str, cfg)`: binds `reason` as is (no `.chars().take(500)`).
- New private `fn stored_reason(e: &otto_resource::Error) -> String` using
  `crate::error::platform_failure_reason`.

`crates/flags-server/tests/log_text.rs`: drop `ALLOWED` and the `allowed` check; the module doc's
mention of exceptions goes with it.

No public interface changes (`stored_reason` is private; `reschedule` is private).

## 7. Invariants impact

- **SDK-reported / remote text untrusted; database error text never reaches a tool response:**
  extended: the platform's text leaves the `error` level and the outbox.
- **Metering:** unchanged. Rows are recorded, shipped, deleted, and moved to
  `usage_outbox_rejected` exactly as before; only the logged fields and the `last_error` value
  change.
- **No SQL outside flags-core; runtime `sqlx::query` only:** the only SQL edit is the bound value;
  the test binary's setup SQL lives in a test, as `e2e.rs` already does.
- **Append-only migrations / rollout compatibility:** no migration. The previous release writes
  text to the same column again after a rollback; harmless.
- **Small shared database:** no new statements on the request path.
- Tenant isolation, MCP auth, SDK contract, rollout bucketing, webhooks, licensing, pin: untouched.

## 8. Migration

None. `last_error` stays `text`, nullable.

## 9. Out-of-band

None. No config key, secret, scope, or platform registration.

## 10. Error handling and edge cases

- `Unauthorized` stores `unauthorized`; `Status` stores `http_status <code>`; transport, decode,
  not-found, invalid-URL store their names. `stored_reason` covers every variant through
  `platform_failure_reason` (exhaustive match there).
- A failing `reschedule` still logs `db_failure_reason` at `warn` and the text at `debug`
  (unchanged).
- A platform reason containing newlines or control characters reaches only the `debug` line and
  the bound `usage_outbox_rejected.reason` parameter.

## 11. Testing approach

- **Unit (`usage.rs` tests):** `stored_reason` for a `Status { 502, body: "secret" }` →
  `"http_status 502"`, `Transport` → `"transport"`, `Unauthorized` → `"unauthorized"`; none
  contains `secret`.
- **Log-capture + DB (`crates/flags-server/tests/usage_logs.rs`, its own binary, one
  `#[sqlx::test(migrator = "flags_core::MIGRATOR")]` test):** a mock platform whose
  `POST /internal/usage` first answers 502 with a body containing a marker, then answers a receipt
  rejecting every event with a reason containing another marker. Insert one `usage_outbox` row;
  `ship_once` → `Err`; assert `last_error = 'http_status 502'` and no marker in it. Make the row
  due again, capture logs around the second `ship_once` → `Ok(1)`; assert the `ERROR` line has the
  `event_id` and `reason="rejected"` and no marker, the `DEBUG` line has the reason marker,
  `usage_outbox_rejected.reason` equals the platform reason, and `usage_outbox` is empty. Own
  binary because `tracing` caches callsite interest (`crates/flags-mcp/tests/failure_logs.rs`).
- **Source scan:** `log_text.rs` with no exceptions passes.
- Gates: test, lint, format, extra (no migration change), and `podman build -t otto-flags:check .`
  (crates changed).

## 12. Risks and open questions

- Operators lose the platform's rejection text at `info`/`error`; it is still at `debug` and in
  `usage_outbox_rejected.reason` (SELECT-only diagnostics). Accepted by the AC.
- The `debug`-level capture relies on the subscriber being the thread default across the test's
  awaits; `#[sqlx::test]` runs a current-thread runtime, so the guard holds.
