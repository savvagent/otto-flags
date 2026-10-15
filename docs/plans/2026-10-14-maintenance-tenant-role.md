# Maintenance tenant role Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `maintenance::begin` drops into the tenant role (`SET LOCAL ROLE`, when assumable, as
`otto_tenant::Db::begin` decides), so every background maintenance transaction is subject to RLS
and the marker policies constrain it in production.

**Architecture:** One statement sets the marker and asks the catalog whether the pool role can
assume `db.tenant_role()` (otto-tenant's own probe query, which is private to it); if so, a second
statement runs `SET LOCAL ROLE`. Callers are unchanged. Tests prove the role and the marker, the
not-assumable fallback, and that every caller still works across orgs on the superuser test pool.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16, otto-tenant at rev `fa1ae08`.

**Ticket:** savvagent/otto-flags#73

**Spec:** `docs/specs/2026-10-14-maintenance-tenant-role-design.md`

## Global Constraints

- Worktree only: `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-73-maintenance-tenant-role`;
  `main` changes only through the PR.
- Gates, from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; run once before the PR)
- Evidence: report pass counts per test binary. `cargo test` without `DATABASE_URL` panics
  ("DATABASE_URL must be set"): that is a missing variable, not a test failure. `DATABASE_URL`
  points only at the local Postgres on 15434. A skipped or ignored test is not a pass. Do not run
  DB tests concurrently with another worktree's.
- Invariants that apply: tenant isolation is RLS under `otto_app`, proven; unpinned cross-org
  access only through `flags_core::maintenance::begin`; no SQL outside flags-core and runtime
  queries only (no `query!`); small shared database (no extra connection, no fan-out); no
  migration (append-only rule untouched).
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #73`. No attribution of any kind (no Co-Authored-By, no
  "Generated with", no credit markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/maintenance.rs` | `begin` drops into the tenant role; module doc; in-crate DB unit tests |
| **Modify.** `crates/flags-core/tests/db.rs` | Default-pool, two-org test of the retention sweeps |
| **Modify.** `crates/flags-server/tests/e2e.rs` | Default-pool, two-org test of `ship_once`, accepted and rejected |
| **Modify.** `docs/deploy/fly.md` | RLS note: the marker policies apply in production |
| **Modify.** `CLAUDE.md` | Invariant line: `maintenance::begin` drops into `otto_app` |

## Task Order & Rationale

Task 1 is the change and its direct proof. Task 2 adds the caller-level regression tests (they pass
before Task 1 too, since a superuser sees every org; after Task 1 they prove the callers still see
across orgs as `otto_app`, which Task 1's unit test shows they now run as). Task 3 updates the docs
to the new behaviour.

---

### Task 1: `maintenance::begin` assumes the tenant role

**Files:**
- Modify: `crates/flags-core/src/maintenance.rs` (whole file, 31 lines today)

**Interfaces:** `pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>>`
keeps its signature; callers in `platform_events.rs`, `telemetry.rs`, `rollback.rs`, `usage.rs`
are not touched.

**Out-of-band / invariants:** none out-of-band. Tenant isolation under `otto_app`; runtime
queries only; the role name is interpolated only from `db.tenant_role()` (validated by
otto-tenant).

**Model:** sonnet — small, fully specified change.

- [ ] **Step 1: Write the failing tests.** Append to `crates/flags-core/src/maintenance.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    /// The current role and the maintenance marker inside `tx`.
    async fn role_and_marker(tx: &mut Transaction<'static, Postgres>) -> (String, String) {
        sqlx::query_as(
            "SELECT current_user::text, coalesce(current_setting('app.maintenance', true), '')",
        )
        .fetch_one(&mut **tx)
        .await
        .unwrap()
    }

    /// The test pool connects as a superuser, as production's attach role
    /// does, which row-level security does not apply to. A maintenance
    /// transaction must drop into the tenant role so the marker policies are
    /// what let it through (#73).
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_runs_as_the_tenant_role_with_the_marker(pool: PgPool) {
        let db = Db::from_pool(pool);
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(
            role_and_marker(&mut tx).await,
            (db.tenant_role().to_string(), "on".to_string())
        );
        // Refused what the tenant role is refused: it has no DELETE on the
        // append-only audit trail, which the pool's superuser would have.
        let err = sqlx::query("DELETE FROM audit_events")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        let code = err.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned());
        assert_eq!(code.as_deref(), Some("42501"), "{err}");
    }

    /// Where the tenant role cannot be assumed, `begin` skips the role change,
    /// as `Db::begin` does, instead of failing every maintenance transaction.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_keeps_the_pool_role_when_the_tenant_role_is_not_assumable(pool: PgPool) {
        let pool_role: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&pool)
            .await
            .unwrap();
        let db = Db::from_pool(pool)
            .with_tenant_role("flags_no_such_role")
            .unwrap();
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(role_and_marker(&mut tx).await, (pool_role, "on".to_string()));
    }
}
```

- [ ] **Step 2: Run the tests to verify the first fails.**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --lib maintenance`
Expected: `begin_runs_as_the_tenant_role_with_the_marker` FAILS (current_user is `flags`, not
`otto_app`); `begin_keeps_the_pool_role_when_the_tenant_role_is_not_assumable` passes. Neither
is skipped.

- [ ] **Step 3: Implement.** Replace `begin` and extend the module doc in
  `crates/flags-core/src/maintenance.rs`:

Module doc: after the paragraph ending "require the marker only when unpinned.", add:

```rust
//!
//! The policies bind only a role row-level security applies to, and the pool
//! may connect as one it does not (production's attach role is a superuser).
//! So [`begin`] drops into the tenant role with `SET LOCAL ROLE`, as
//! `otto_tenant::Db::begin` does for pinned transactions (#73): the unpinned
//! policies are then what let maintenance code see across orgs.
```

`begin`:

```rust
/// Open an unpinned transaction on `db`'s pool, as the tenant role
/// (`db.tenant_role()`) when the pool's role can assume it, with the
/// maintenance marker set. Both are `SET LOCAL`, so they end with the
/// transaction and never leak to the next user of the connection.
///
/// Whether the role can be assumed is the question `otto_tenant::Db::begin`
/// asks before its own `SET LOCAL ROLE`, with the same catalog query: its
/// answer is private to otto-tenant, so it is asked again here, once per
/// transaction. Where it cannot be assumed the role is left alone, as
/// `Db::begin` leaves it; the server only starts when
/// `Db::verify_tenant_isolation` has shown the pool's own role is then subject
/// to row-level security.
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = db.pool().begin().await?;
    // Two columns, not one `AND`: Postgres may skip either side of an `AND`,
    // and the marker must be set either way.
    let (_, assumable): (String, bool) = sqlx::query_as(
        "SELECT set_config('app.maintenance', 'on', true), EXISTS ( \
           SELECT 1 FROM pg_catalog.pg_roles r \
           WHERE r.rolname = $1 \
             AND pg_catalog.pg_has_role(current_user, r.oid, 'USAGE'))",
    )
    .bind(db.tenant_role())
    .fetch_one(&mut *tx)
    .await?;
    if assumable {
        // `tenant_role` is validated by otto-tenant (`^[a-z_][a-z0-9_]*$`)
        // before it can be set; `SET ROLE` takes no bind parameter.
        sqlx::query(&format!("SET LOCAL ROLE {}", db.tenant_role()))
            .execute(&mut *tx)
            .await?;
    }
    Ok(tx)
}
```

- [ ] **Step 4: Run the tests to verify they pass, then the full gates.**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --lib maintenance`
Expected: 2 passed, 0 skipped.

Then: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`. Every
existing test that runs a maintenance caller (sweeps, `due_policies`, `ship_once`) must still pass:
they now run as `otto_app`.

- [ ] **Step 5: Commit.**

```bash
git add crates/flags-core/src/maintenance.rs
git commit -m "fix(core): run maintenance transactions as the tenant role

The marker policies bind only a role row-level security applies to, and
production's pool connects as a superuser, so background sweeps, the
checker's backstop, and the usage shipper ran RLS-exempt. Drop into the
tenant role as pinned transactions do, when it can be assumed.

Fixes #73"
```

---

### Task 2: Callers still see across orgs on the superuser pool

**Files:**
- Modify: `crates/flags-core/tests/db.rs` (new test after `unpinned_sweeps_delete_as_otto_app`)
- Modify: `crates/flags-server/tests/e2e.rs` (new test after `a_rejected_usage_event_keeps_a_capped_reason`)

**Interfaces:** consumes Task 1's `begin`; uses existing helpers `db`, `policy_fixture`,
`ERROR_RETENTION_DAYS`, `EVAL_RETENTION_DAYS`, `telemetry`, `platform_events` (db.rs) and
`start_platform`, `server`, `platform.lock().unwrap().reject_with` / `.shipped` (e2e.rs). Read
`unpinned_sweeps_delete_as_otto_app` in db.rs first: it is the fixture pattern to follow.
`due_policies` across orgs on the default pool is already covered by
`due_policies_sees_every_org_unpinned`; do not duplicate it.

**Out-of-band / invariants:** none.

**Model:** sonnet.

- [ ] **Step 1: Add the db test.** In `crates/flags-core/tests/db.rs`:

```rust
/// The retention sweeps on the default test pool, whose superuser role RLS
/// does not apply to: maintenance transactions drop into `otto_app` (#73), so
/// this proves the marker policies, not the pool role, let them reach every
/// org's expired rows (and only those).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn sweeps_reach_every_org_on_the_superuser_pool(pool: PgPool) {
    let db = db(pool.clone()).await;
    let actor = UserId::new();
    let keep_days = 30;
    let orgs = [OrgId::new(), OrgId::new()];
    for org in orgs {
        let (app, flag) = policy_fixture(&db, org, actor).await;
        let mut tx = db.begin(org).await.unwrap();
        let policy = tx
            .set_policy(&app, &flag, "production", Mode::Enforce, Thresholds::default(), actor)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        for hours in [(ERROR_RETENTION_DAYS + 1) * 24, 1] {
            sqlx::query(
                "INSERT INTO flag_errors (org_id, flag_id, flag_enabled, error_type, error_message, \
                 occurred_at, received_at) \
                 VALUES ($1, $2, true, 'e', 'm', now(), now() - make_interval(hours => $3))",
            )
            .bind(org)
            .bind(flag.id)
            .bind(hours)
            .execute(&pool)
            .await
            .unwrap();
        }
        for days in [EVAL_RETENTION_DAYS + 1, 0] {
            sqlx::query(
                "INSERT INTO flag_eval_hourly (org_id, flag_id, environment, hour, enabled, count, server_count) \
                 VALUES ($1, $2, 'production', date_trunc('hour', now()) - make_interval(days => $3), true, 1, 1)",
            )
            .bind(org)
            .bind(flag.id)
            .bind(days)
            .execute(&pool)
            .await
            .unwrap();
        }
        for days in [EVAL_RETENTION_DAYS + 1, 1] {
            sqlx::query(
                "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed, created_at) \
                 VALUES ($1, $2, $3, 'production', 'rolled_back', '{}'::jsonb, now() - make_interval(days => $4))",
            )
            .bind(org)
            .bind(policy.policy.id)
            .bind(flag.id)
            .bind(days)
            .execute(&pool)
            .await
            .unwrap();
        }
        for days in [keep_days + 1, 1] {
            sqlx::query(
                "INSERT INTO platform_events (event_id, org_id, kind, received_at) \
                 VALUES ($1, $2, 'member.removed', now() - make_interval(days => $3))",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(org)
            .bind(days)
            .execute(&pool)
            .await
            .unwrap();
        }
    }

    assert_eq!(telemetry::sweep(&db).await.unwrap(), 6);
    assert_eq!(platform_events::sweep(&db, keep_days).await.unwrap(), 2);
    for table in ["flag_errors", "flag_eval_hourly", "rollback_events", "platform_events"] {
        // Uuid orders bytewise, as Postgres orders uuid; OrgId has no Ord.
        let orgs_left: Vec<uuid::Uuid> =
            sqlx::query_scalar(&format!("SELECT org_id FROM {table} ORDER BY org_id"))
                .fetch_all(&pool)
                .await
                .unwrap();
        let mut expected: Vec<uuid::Uuid> = orgs.iter().map(|o| o.as_uuid()).collect();
        expected.sort();
        assert_eq!(orgs_left, expected, "{table}: each org keeps its fresh row");
    }
}
```

If `policy_fixture`'s signature or the `set_policy` arguments differ
from the above, adapt to what the file uses (the existing `unpinned_sweeps_delete_as_otto_app` is
authoritative) without changing what is asserted.

- [ ] **Step 2: Add the e2e test.** In `crates/flags-server/tests/e2e.rs`:

```rust
/// The shipper on the default test pool, whose superuser role RLS does not
/// apply to: its maintenance transactions drop into `otto_app` (#73), so this
/// proves the outbox's marker policies let it claim, delete, and record
/// rejections for every org.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_shipper_reaches_every_org_on_the_superuser_pool(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let insert = |org: Uuid| {
        let pool = s.db.pool().clone();
        async move {
            sqlx::query(
                "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
                 VALUES ($1, NULL, 'create_flag', true)",
            )
            .bind(org)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let count = |table: &'static str| {
        let pool = s.db.pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(DISTINCT org_id) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

    insert(a).await;
    insert(b).await;
    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(platform.lock().unwrap().shipped, 2);
    assert_eq!(count("usage_outbox").await, 0);

    platform.lock().unwrap().reject_with = Some("unknown org".into());
    insert(a).await;
    insert(b).await;
    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(count("usage_outbox").await, 0);
    assert_eq!(count("usage_outbox_rejected").await, 2);
}
```

- [ ] **Step 3: Run both tests.**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db sweeps_reach_every_org_on_the_superuser_pool`
and `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e the_shipper_reaches_every_org_on_the_superuser_pool`
Expected: 1 passed each, not skipped. (These pass with or without Task 1 — a superuser sees every
org — so there is no red step; Task 1's unit test is what shows they now run as `otto_app`.)

- [ ] **Step 4: Full gates.** Run `cargo fmt --all` first (rustfmt will re-wrap these snippets),
  then test, lint, and format as in Global Constraints.

- [ ] **Step 5: Commit.**

```bash
git add crates/flags-core/tests/db.rs crates/flags-server/tests/e2e.rs
git commit -m "test: prove maintenance callers reach every org as the tenant role

The sweeps and the usage shipper now run as otto_app even on the
superuser test pool, so two-org tests there show the marker policies
still let them reach every org.

Fixes #73"
```

---

### Task 3: Docs say the marker policies apply in production

**Files:**
- Modify: `docs/deploy/fly.md` (the bullet beginning "- Migration `0003` (#12) gives the retention
  sweeps", around lines 335–345)
- Modify: `CLAUDE.md` (Invariants, the bullet "**Request paths open transactions with
  `platform_events::begin_live`**")

**Out-of-band / invariants:** none.

**Model:** haiku — text replacement given verbatim.

- [ ] **Step 1: Replace the fly.md bullet** (from "- Migration `0003` (#12)" through "transaction
  to see across orgs.") with:

```markdown
- Migration `0003` (#12) gives the retention sweeps an unpinned read so they work under a role
  subject to RLS. Every unpinned policy added by `0002` and `0003` also requires the
  transaction-local marker `app.maintenance = 'on'` (#27), which only the background sweeps, the
  checker's backstop, and the usage shipper set. Migration `0005` (#68) does the same for
  `usage_outbox` and `usage_outbox_rejected`, for every command. Those background transactions
  (`maintenance::begin`) drop into `otto_app` with `SET LOCAL ROLE`, as tenant transactions do
  (#73), so in production too the marker policies are what let them see across orgs. A query
  run directly on the pool without pinning an org still runs as the superuser attach role, which
  RLS does not apply to; request paths pin through `begin_live`. A release from before #73 runs
  its background work as the attach role, so it keeps working during the rollout overlap and
  after a rollback. Under a pool role subject to RLS, a release from before `0005` ships no usage
  while it runs against that schema, and the rows wait in the outbox until a newer release ships
  them. Ad-hoc diagnostics as a role subject to RLS need
  `SET LOCAL app.maintenance = 'on'` in their transaction to see across orgs.
```

- [ ] **Step 2: In CLAUDE.md**, change "the usage shipper), which sets the\n  `app.maintenance`
  marker the unpinned RLS policies require." to "the usage shipper), which drops into `otto_app`
  and sets the `app.maintenance` marker the unpinned RLS policies require." Re-wrap to the
  file's ~100-column width.

- [ ] **Step 3: Gates.** `cargo fmt --all -- --check`; the migrations extra gate
  (`git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints
  nothing); `podman build -t otto-flags:check .` (the image builds with Task 1's change).

- [ ] **Step 4: Commit.**

```bash
git add docs/deploy/fly.md CLAUDE.md
git commit -m "docs: maintenance transactions run as otto_app

The RLS notes said the marker changes nothing in production; it now
does, because maintenance transactions drop into the tenant role.

Fixes #73"
```
