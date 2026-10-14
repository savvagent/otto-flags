# Batch error telemetry into one INSERT

**Status:** DRAFT
**Issue:** #23

## 1. Brief

From #23 (found in code review of #11, Task 3):

> `flags_core::telemetry::record_errors` runs one `INSERT INTO flag_errors` per event in the batch,
> so a full batch (`MAX_BATCH` = 1000) is up to 1000 round trips inside one transaction on the SDK
> request path. `record_evaluations` already aggregates its batch and writes it with one `UNNEST`
> upsert.
>
> The repo's invariants rule out per-row query fan-out on the small shared database (5 pool
> connections on a 512 MB instance): a burst of error telemetry holds a pooled connection for the
> whole loop.
>
> **Acceptance criteria**
> - [ ] `record_errors` writes a batch with a single statement (bound arrays + `UNNEST`, as
>   `record_evaluations` does), keeping truncation, the `occurred_at` sanity bound, the `reporter`
>   column, and the `Receipt` / `Recorded` results unchanged.
> - [ ] The existing telemetry tests in `crates/flags-core/tests/db.rs` pass unchanged, plus one that
>   records a multi-event error batch and checks every row landed.

## 2. Premise corrections

None: `record_errors` (`crates/flags-core/src/telemetry.rs`) does loop one `INSERT` per known
event, and its only production caller is the SDK error endpoint (`crates/flags-api/src/telemetry.rs`),
inside the request's transaction.

## 3. Assumptions

- **Not aggregated.** Errors stay one row per event (they are kept raw, unlike evaluations), so the
  batch becomes parallel arrays, not buckets. Events naming unknown flags are dropped and counted
  exactly as now.
- **NULLs through arrays.** `environment` and `stack_trace` are nullable; they bind as
  `Vec<Option<String>>` (`text[]` with NULL elements), so a missing value stays NULL rather than
  becoming an empty string.
- **`reporter` and `org_id` are scalars** bound once (`$1`, last parameter), as `record_evaluations`
  binds `org_id`: every row of a batch shares them.
- **Empty batch:** when no event names a known flag, no statement runs (as `record_evaluations`).
- **Row order:** `INSERT … SELECT … FROM UNNEST(…)` assigns `id`s in array order in practice; nothing
  reads `flag_errors` by `id`, so order is not relied on. `received_at` keeps its column default.
- **Statement size:** a full batch at every field's cap is about 1000 × ~10 KB ≈ 10 MB of bound
  text in one statement, the same bytes the loop sent across 1000 statements. The API already
  bounds the request body; no new bound is needed.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

One round trip writes an error batch of any size up to `MAX_BATCH`.

- `record_errors` issues exactly two statements per call with known flags (the key lookup and one
  `INSERT`), and one with none.
- Truncation (`environment` 64, `error_type` 200, `error_message` 2,000, `stack_trace` 8,000
  characters), `sane(occurred_at)`, `reporter`, `Receipt`, and `Recorded.flags` (deduplicated,
  sorted) are unchanged.
- Every existing telemetry test in `crates/flags-core/tests/db.rs` passes unchanged.
- A new test records a mixed batch (several flags, NULL and non-NULL `environment` and
  `stack_trace`, an unknown key, an over-long message, a future timestamp) and checks every known
  event landed as its own row with the right values, and the receipt.

## 5. Scope

**In:** `record_errors` in `crates/flags-core/src/telemetry.rs`; one new test in
`crates/flags-core/tests/db.rs`.

**Out:** the API handler, the request-body limits, `record_evaluations`, retention, schema.

## 6. Architecture

`record_errors` keeps its signature. After resolving keys (unchanged), it builds parallel vectors
(`flag_id: Vec<Uuid>`, `environment: Vec<Option<String>>`, `flag_enabled: Vec<bool>`,
`error_type: Vec<String>`, `error_message: Vec<String>`, `stack_trace: Vec<Option<String>>`,
`occurred_at: Vec<DateTime<Utc>>`), applying the same truncation and `sane` as today, then runs:

```sql
INSERT INTO flag_errors
  (org_id, flag_id, environment, flag_enabled, error_type, error_message, stack_trace, occurred_at, reporter)
SELECT $1, f, env, en, et, em, st, at, $9
  FROM UNNEST($2::uuid[], $3::text[], $4::bool[], $5::text[], $6::text[], $7::text[], $8::timestamptz[])
    AS t(f, env, en, et, em, st, at)
```

through `tx.conn()` in the caller's tenant transaction, so RLS (`org_id = current_org()` on
insert) and the composite `(org_id, flag_id)` foreign key apply as before. No public interface
changes.

## 7. Invariants impact

- **The database is small and shared / no per-row fan-out:** this is the fix; one statement per
  batch.
- **Tenant isolation:** unchanged. The insert still runs pinned in the request's `Tx`, with
  `org_id` from `tx.org()`, never from the request; the flag ids come from `flag_ids`, which is
  scoped to the org and the authenticated app.
- **No SQL outside flags-core; runtime `sqlx::query` only:** holds.
- **SDK-reported text is untrusted:** stored exactly as before (truncated, bound as parameters,
  never interpolated).
- **Evaluations are counted, not stored; errors raw and truncated (14 days):** unchanged.
- **SDK REST contract:** unchanged (same receipt).

## 8. Migration

None.

## 9. Out-of-band

None. Deploy is CI-auto; verification is the standard health and boot-log checks plus a read-only
check of the changed surface. The SDK error endpoint is a write, so it is not exercised against
production; `flag_health` (a read) is.

## 10. Error handling and edge cases

- A failure fails the whole batch, as it did when any one insert in the loop failed (same
  transaction).
- Text with characters Postgres rejects (NUL) fails the batch as it failed the loop: unchanged.
- Unknown keys only: no INSERT, receipt counts them.

## 11. Testing approach

- `crates/flags-core/tests/db.rs`, `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`: the new
  mixed-batch test in §4. Existing tests (the 40-event health test, the reporter test, the
  rollback-checker tests that record errors) cover the rest unchanged.
- Gates: test (with `DATABASE_URL`), clippy, fmt, the migrations guard, and `podman build`.

## 12. Risks and open questions

- None beyond §3's statement size, which is unchanged in total bytes.
