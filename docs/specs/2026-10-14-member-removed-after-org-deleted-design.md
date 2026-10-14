# Skip the audit write for a member removed from a deleted org

**Status:** DRAFT
**Issue:** #41

## 1. Brief

From #41 (found by the security review of #39 (#21); the behaviour predates it):

> `member_removed` (`crates/flags-core/src/platform_events.rs`) writes an audit row (`MEMBER_REMOVED`)
> inside a transaction pinned to the org, and it does not check `deleted_orgs`. If a
> `member.removed` for an org arrives after that org's `org.deleted` has been applied, the row
> lands in an already-purged org's audit trail. It stays there, because the purge only runs again
> on a redelivered `org.deleted`.
>
> The impact is small: one row per late event, holding the removed user's id, for an org that no
> longer exists. But it is data about a deleted org kept indefinitely.
>
> **Acceptance criteria**
> - [ ] `member_removed` skips the audit write (and records the delivery as ignored) when the org
>   is tombstoned in `deleted_orgs`.
> - [ ] A DB test applies `org.deleted`, then a `member.removed` for the same org, and asserts no
>   audit row remains for that org.

## 2. Premise corrections

None. The brief matches the code: `member_removed` (`platform_events.rs`, the last function)
inserts into `removed_members`, opens `db.begin(org)`, takes the org's lifecycle lock exclusively
(`exclude_requests`), records the delivery (`first_delivery`), and on a first delivery writes
`tx.audit(MEMBER_REMOVED)`, with no look at `deleted_orgs`.

## 3. Assumptions

- **"Records the delivery as ignored" means:** the dedupe marker is still written to
  `platform_events` (kind `member.removed`), so a redelivery is recognised, and the webhook answers
  `Outcome::Ignored` on the first delivery and `Outcome::Duplicate` on a redelivery, the same
  pairing `team.deleted` uses. `platform_events` rows for a deleted org already survive the purge
  ("only the dedupe marker and the tombstone survive", module doc) and are swept by age, so one
  more marker is not new retention.
- **The tombstone check reuses `revoked_on(conn, org, None)`**, the single existing definition of
  "this org is deleted" (with `user = None` its `removed_members` arm cannot match), rather than a
  second `deleted_orgs` query.
- **The `removed_members` insert is left as is.** It runs before the transaction, is keyed by ids
  only, expires after `REMOVED_MEMBER_TTL_SECS` and is swept at twice that; it is bounded, unlike
  the audit row. Changing it is outside #41.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

A `member.removed` that arrives after its org's `org.deleted` leaves no audit row for that org.

- After `org.deleted` then `member.removed` for the same org, `audit_events` holds no row for the
  org, and `apply` returns `Outcome::Ignored`.
- Redelivering that `member.removed` returns `Outcome::Duplicate` and still writes nothing to
  `audit_events`.
- A `member.removed` for a live org behaves exactly as before (`Applied`, one `MEMBER_REMOVED`
  audit row, the member refused); existing tests pass unchanged.
- Holds when the pool runs as `otto_app` (production's role).

## 5. Scope

**In:** `member_removed` and the module doc's event table in
`crates/flags-core/src/platform_events.rs`; tests in `crates/flags-core/tests/db.rs`.

**Out:** `org_deleted`, `team.deleted`, the `removed_members` insert, the webhook handler
(`crates/flags-api/src/platform.rs`, which serialises whatever `Outcome` it gets), schema.

## 6. Architecture

In `member_removed`, after `exclude_requests` (so the check runs under the org's exclusive
lifecycle lock) and after `first_delivery` (so the marker is recorded either way):

```rust
let deleted = revoked_on(tx.conn(), org, None).await?;
if first && !deleted { tx.audit(...).await?; }
tx.commit().await?;
```

The outcome becomes `Ignored` when `deleted && first`, `Duplicate` when `!first`, `Applied`
otherwise. The info log distinguishes the deleted-org case ("member.removed for a deleted org;
ignored") from the normal one. The module doc's `member.removed` row adds: "For an org already
deleted, nothing is written but the dedupe marker (#41)."

Ordering against a concurrent `org.deleted`: `org_deleted` commits its tombstone on its own, then
takes the same lock exclusively for the purge. If `member_removed` holds the lock first and the
tombstone is already committed, it sees it (read committed, statement after the lock) and skips;
if the tombstone is not yet committed, it writes the row, and the purge, which takes the lock only
after its tombstone and after this transaction commits, deletes it. Either way no row survives.

No public interface changes: `Outcome` already has `Ignored`; the webhook response shape is
unchanged.

## 7. Invariants impact

- **Platform webhooks signature-verified, fail closed, idempotent:** verification untouched; the
  dedupe marker is still written in the same transaction, so redelivery is idempotent
  (`Duplicate`, no write). A database error still fails the delivery (503, redelivered).
- **Tenant isolation (RLS under `otto_app`):** the new read is the existing `revoked_on` query on
  `deleted_orgs`, which `begin_live` already runs inside pinned `otto_app` transactions.
- **No SQL outside flags-core; runtime `sqlx::query` only:** unchanged (no new statement text).
- **Request paths open with `begin_live`:** not a request path; `member_removed` keeps
  `db.begin(org)` plus the exclusive lock, as before.
- **Small shared database:** one extra indexed `EXISTS` per `member.removed` delivery.

## 8. Migration

None.

## 9. Out-of-band

None. Deploy is CI-auto. The webhook is a write, so production verification is health, boot logs,
and a read-only MCP `whoami`; the late-event path is proven by the DB test, not exercised in
production.

## 10. Error handling and edge cases

- `member.removed` for an org never seen here (no tombstone, no data): unchanged, `Applied`.
- `member.removed` redelivered after `org.deleted`, whose first delivery was before it: the first
  delivery wrote a row the purge then deleted; the redelivery is `Duplicate` and writes nothing.
- Any database error in the new read fails the delivery like every other statement here.

## 11. Testing approach

- `crates/flags-core/tests/db.rs`, `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`,
  `member_removed_after_org_deleted_writes_no_audit_row`, using `otto_app_db`: create an app in the
  org (audit rows exist), apply `org.deleted`, apply `member.removed` for the org (`Ignored`),
  `audit_rows(&pool, org) == 0`, redeliver (`Duplicate`), still 0, and the `platform_events` marker
  for the event exists.
- Extend `a_removed_member_is_refused_but_others_are_not` to assert its outcome is `Applied` and
  that exactly one `platform.member.removed` audit row exists for the org, pinning the live-org
  behaviour.
- Gates: test (with `DATABASE_URL`), clippy, fmt, the migrations guard, and `podman build`.

## 12. Risks and open questions

None open.
