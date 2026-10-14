# Cap the stored platform rejection reason

**Status:** DRAFT
**Issue:** #51

## 1. Brief

From #51 (found reviewing #50 (#44)):

> `crates/flags-core/src/usage.rs` (`ship_once`) inserts the platform's per-event rejection reason into `usage_outbox_rejected.reason` with no length limit (`text NOT NULL` in `0001_baseline.sql`). The 500-character cap that used to apply to `usage_outbox.last_error` never covered this column, and since #50 `last_error` holds only a short name, so this is the one place remote text is still stored unbounded. Nothing in `crates/` reads the column back, so it never reaches a tool or SDK response; the concern is growth on the small shared database if the platform misbehaves.
>
> **Acceptance criteria**
> - [ ] The rejection reason is capped (for example 500 characters, on a character boundary) when it is bound into `usage_outbox_rejected`; no migration (the column type is unchanged).
> - [ ] A test proves an over-long reason is stored truncated.

## 2. Premise corrections

- **Length is not the only way the platform's text can hurt this insert.** A reason containing NUL
  (valid in JSON and in a Rust `String`) fails the `INSERT` (`invalid byte sequence for encoding
  "UTF8": 0x00`), which fails the whole `ship_once` transaction after the platform has already
  answered. Nothing is deleted, the rows come due again after the claim lease, the platform rejects
  them again, and the shipper loops on that batch, holding up every row behind it. The same helper
  that caps the reason replaces NUL, as #38 did for SDK telemetry text.

## 3. Assumptions

- **500 characters**, as the issue suggests and as `last_error` was capped before #50. Enough for
  any reason an operator needs; a platform reason is a short sentence.
- **Reuse `telemetry::stored`** (cap by characters, NUL → U+FFFD), made `pub(crate)`, rather than
  a second copy in `usage`. It is the crate's one rule for "remote text bound into a `text`
  column"; its doc comment says so. Moving it to a new module is not worth the churn for one more
  caller.
- **Truncate silently**, no ellipsis: the column is for operators, and the cap is documented
  beside the constant.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

The platform's rejection reason is stored bounded and always storable.

- A 600-character reason is stored as its first 500 characters.
- A reason containing NUL is stored with U+FFFD in its place, and the batch completes (the event
  moves to `usage_outbox_rejected`, the outbox row is deleted).
- No migration; the column is unchanged.

## 5. Scope

**In:** `ship_once`'s insert into `usage_outbox_rejected`; `telemetry::stored` visibility and doc.

**Out:** existing rows (nothing reads them; a cleanup would be a production write); the `debug`
log of the platform's reason (logs are not the database, and #44 already keeps it at `debug`).

## 6. Architecture

- `crates/flags-core/src/usage.rs`: `const MAX_REJECTED_REASON: usize = 500;` with a doc comment,
  and `.bind(crate::telemetry::stored(&r.reason, MAX_REJECTED_REASON))` in place of
  `.bind(&r.reason)`. The comment above the insert notes the cap and NUL.
- `crates/flags-core/src/telemetry.rs`: `fn stored` becomes `pub(crate) fn stored`; its doc comment
  (add one if missing) says it is how remote text is bound into a `text` column anywhere in the
  crate: capped by characters, NUL replaced.

No public interface changes.

## 7. Invariants impact

- **The database is small and shared / no unbounded tables:** strengthened; the one remaining
  unbounded remote text column is capped. The table's row count is bounded by what the platform
  rejects, which is unchanged.
- **Append-only migrations / compatible with the previous release:** no migration.
- **No SQL outside flags-core:** unchanged.

## 8. Migration

None.

## 9. Out-of-band

None.

## 10. Error handling and edge cases

- A reason of exactly 500 characters is stored whole; 501 loses the last one.
- Multi-byte characters: the cap counts `char`s, so no character is split.
- An empty reason stays empty (the column is `NOT NULL`, not non-empty).

## 11. Testing approach

e2e (`crates/flags-server/tests/e2e.rs`, against its mock platform), because shipping needs the
platform's receipt: the mock `Platform` gains `reject_with: Option<String>`; when set, `ship`
rejects every event with that reason. New test `a_rejected_usage_event_keeps_a_capped_reason`:
insert one `usage_outbox` row, set `reject_with` to `"\0" + "é".repeat(599)` (600 characters,
multi-byte, with NUL), `ship_once` returns `Ok(1)`, and the stored reason equals
`"\u{FFFD}" + "é".repeat(499)` (500 characters); `usage_outbox` is empty.

Gates: test, lint, format, extra, and the `crates/**` podman build.

## 12. Risks and open questions

- None material.
