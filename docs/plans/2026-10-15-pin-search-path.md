# Pin the search_path of flags-core transactions Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Every transaction flags-core opens resolves unqualified names in `public` only, so a
schema named after the tenant role cannot redirect a query (#97).

**Architecture:** A `pub(crate) const SEARCH_PATH` in `unpinned.rs`, applied with a
transaction-local `set_config('search_path', …, true)` folded into the existing probe statement
(unpinned and maintenance transactions) and the existing lifecycle-lock statement
(`begin_live`); the remaining pinned transactions go through a new `begin_pinned` helper; a
source scan keeps it that way.

**Tech Stack:** Rust, sqlx (runtime queries only), Postgres 16, otto-tenant.

**Ticket:** savvagent/otto-flags#97

**Spec:** `docs/specs/2026-10-15-pin-search-path-design.md`

## As built

Review of #116 split two tests and added four: the unpinned lookup test became
`revocation_checks_…` and `key_resolution_…`, the request test became
`request_tombstone_checks_…` and `request_reads_…`, the webhook test became one per event
(`team_deleted_…`, `org_deleted_…`, `member_removed_…`), `transactions_ignore_a_shadowing_temporary_table`
covers `pg_temp`, and the scan also matches `Db::begin(`. The probe's extra tuple slot is `String`.
The spec's Testing approach lists the final set.

## Global Constraints

- Gates, from the worktree `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-97-pin-search-path`,
  always with `export CARGO_BUILD_JOBS=3` and `mkdir -p target/tmp && export TMPDIR=$PWD/target/tmp`:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15448/otto_flags cargo test --workspace`
    (this run's dedicated Postgres on 15448, never 15434 or any Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - crates changed: `podman build -t otto-flags:check .` (final run only)
- Evidence: report pass counts per test binary. A `DATABASE_URL must be set` panic is a missing
  variable, not a pass. A panic inside sqlx-core `testing/mod.rs` during test-database setup is
  environmental: re-run before treating it as a failure.
- Invariants: tenant isolation is RLS under `otto_app` (no policy, grant, or role change here);
  no SQL outside `flags-core`, runtime `sqlx::query*` only, never `query!`; request paths use
  `begin_live`; no migration; the database is small and shared (no new round trip on the SDK key,
  request, or maintenance paths).
- Out-of-band: none. Never run `ALTER ROLE` or any production write.
- Worktree only; trunk changes only through the PR.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #97`; no attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/unpinned.rs` | `SEARCH_PATH`; set it in the probe statement; unit test |
| **Modify.** `crates/flags-core/src/maintenance.rs` | Test: the path ends with the transaction |
| **Modify.** `crates/flags-core/src/platform_events.rs` | Fold the setting into `begin_live`'s lock statement; new `begin_pinned`; webhook handlers use it |
| **Modify.** `crates/flags-core/src/audit_purge.rs` | `check` uses `begin_pinned` |
| **Modify.** `crates/flags-core/tests/db.rs` | Shadow-schema fixture and four tests; source scan for `db.begin(` |

## Task Order & Rationale

Task 1 makes the behavior change test-first (the shadow tests are the AC). Task 2 adds the
guard that keeps future pinned transactions on the helpers; it depends on Task 1's helper.

---

### Task 1: Pin search_path in every flags-core transaction

**Files:**
- Modify: `crates/flags-core/src/unpinned.rs:50-117` (probe statement), tests module
- Modify: `crates/flags-core/src/maintenance.rs:134-166` (test only)
- Modify: `crates/flags-core/src/platform_events.rs:94-105, 148, 192, 267`
- Modify: `crates/flags-core/src/audit_purge.rs:217-224`
- Test: `crates/flags-core/tests/db.rs` (append near the other `platform_events` tests, before
  `no_raw_pool_queries_outside_the_allowed_sites`)

**Interfaces:** produces `crate::unpinned::SEARCH_PATH` and
`crate::platform_events::begin_pinned(db: &Db, org: OrgId) -> Result<Tx<'static>>` (both
`pub(crate)`), used by Task 2's scan.

**Out-of-band / invariants:** none out-of-band. Tenant isolation: role switch and marker logic
unchanged. No new round trip on `unpinned::begin`, `maintenance::begin`, or `begin_live`.

**Model:** sonnet — specified change, some SQL care.

- [x] **Step 1: Write the failing DB tests** in `crates/flags-core/tests/db.rs`:

```rust
/// Plant a schema named after the tenant role, owned by it, holding empty
/// copies of `tables` (defaults and unique constraints included). Under the
/// default `search_path` (`"$user", public`) a transaction running as
/// `otto_app` would resolve those names here instead of in `public` (#97).
async fn shadow_schema(pool: &PgPool, tables: &[&str]) {
    sqlx::query("CREATE SCHEMA otto_app AUTHORIZATION otto_app")
        .execute(pool)
        .await
        .unwrap();
    for t in tables {
        for sql in [
            format!("CREATE TABLE otto_app.{t} (LIKE public.{t} INCLUDING ALL)"),
            format!("ALTER TABLE otto_app.{t} OWNER TO otto_app"),
        ] {
            sqlx::query(&sql).execute(pool).await.unwrap();
        }
    }
}

/// An app in `org`, committed, and its client key.
async fn app_with_key(db: &Db, org: OrgId) -> String {
    let mut tx = db.begin(org).await.unwrap();
    let (_, issued) = tx
        .create_app("web", &["production".into()], UserId::new())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    issued.client_key.unwrap()
}

/// Unpinned lookups (key resolution, the revocation check) read `public`, not
/// a shadowing `otto_app` schema that says the org is deleted and holds no keys.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn unpinned_lookups_ignore_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    let key = app_with_key(&db, org).await;
    shadow_schema(&pool, &["deleted_orgs", "app_keys"]).await;
    sqlx::query("INSERT INTO otto_app.deleted_orgs (org_id) VALUES ($1)")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!platform_events::revoked(&db, org, UserId::new()).await.unwrap());
    let owner = keys::resolve(&db, &key).await.unwrap().expect("resolves in public");
    assert_eq!(owner.org_id, org);
}

/// A request transaction re-checks the tombstones and reads tenant tables in
/// `public`, not in a shadowing schema.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn request_transactions_ignore_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    app_with_key(&db, org).await;
    shadow_schema(&pool, &["deleted_orgs", "flag_apps"]).await;
    sqlx::query("INSERT INTO otto_app.deleted_orgs (org_id) VALUES ($1)")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = begin_live(&db, org, None)
        .await
        .expect("the only tombstone is in the shadow schema");
    let names: Vec<String> = tx
        .list_apps()
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert_eq!(names, ["web"]);
}

/// A maintenance sweep deletes from `public`, not from a shadowing schema.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn maintenance_transactions_ignore_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    shadow_schema(&pool, &["platform_events"]).await;
    for schema in ["public", "otto_app"] {
        sqlx::query(&format!(
            "INSERT INTO {schema}.platform_events (event_id, org_id, kind, received_at) \
             VALUES (gen_random_uuid(), gen_random_uuid(), 'team.deleted', now() - interval '30 days')"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    platform_events::sweep(&db, 7).await.unwrap();
    for (schema, left) in [("public", 0), ("otto_app", 1)] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {schema}.platform_events"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, left, "{schema}");
    }
}

/// A webhook's dedupe marker lands in `public`, so a redelivery is recognised
/// there and the shadow schema stays empty.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn webhooks_ignore_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    shadow_schema(&pool, &["platform_events"]).await;
    let org = OrgId::new();
    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "team.deleted",
        Utc::now(),
        &json!({"org_id": org.as_uuid(), "team_id": uuid::Uuid::new_v4()}),
    );
    let event = otto_resource::webhook::parse(&body).unwrap();
    assert_eq!(platform_events::apply(&db, &event).await.unwrap(), Outcome::Ignored);
    assert_eq!(platform_events::apply(&db, &event).await.unwrap(), Outcome::Duplicate);
    for (schema, n) in [("public", 1), ("otto_app", 0)] {
        let got: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {schema}.platform_events"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(got, n, "{schema}");
    }
}
```

Add `use flags_core::apps::AppsExt;` only if not already imported (it is, line 6).

- [x] **Step 2: Run them and see all four fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15448/otto_flags cargo test -p flags-core --test db shadowing_schema`
Expected: 4 failed (revoked true / AccessRevoked / public count 1 / public count 0), none skipped.

- [x] **Step 3: Implement**

`unpinned.rs`, above `begin`:

```rust
/// The `search_path` every flags-core transaction runs with (#97): unqualified
/// names resolve in `public` after the built-ins, never in a schema named after
/// the current role (`"$user"`, first in the default path) and never in a
/// same-session temporary table (`pg_temp`, otherwise searched first for
/// relations). The same value `0004_audit_purge.sql` gives the purge function.
/// Set transaction-locally, so it ends with the transaction like the role.
pub(crate) const SEARCH_PATH: &str = "pg_catalog, public, pg_temp";
```

In `begin_as_tenant_role`, add one ignored `Option<String>` slot to the decoded tuple, add the
column `set_config('search_path', $3::text, true)` (unconditional, its own column) after the two
CASE columns, and `.bind(SEARCH_PATH)` third. Update the leading comment: one round trip for the
probes, the marker, the role switch, and the search path. Update the fn doc's "Role and marker
are both transaction-local" sentence to name the search path too.

`platform_events.rs`, `begin_live`:

```rust
    sqlx::query(
        "SELECT pg_advisory_xact_lock_shared($1, hashtext($2::text)), \
                set_config('search_path', $3::text, true)",
    )
    .bind(LIFECYCLE_LOCK)
    .bind(org)
    .bind(crate::unpinned::SEARCH_PATH)
    .execute(tx.conn())
    .await?;
```

with a comment: the lock statement also pins the search path (`crate::unpinned::SEARCH_PATH`),
so a request transaction pays no extra round trip for it; the statement's own functions are
built-ins, resolved in `pg_catalog` whatever the path.

New, after `begin_live`:

```rust
/// Open a transaction pinned to `org` with the search path pinned
/// (`crate::unpinned::SEARCH_PATH`). For the webhook handlers and the boot
/// check, which pin an org without `begin_live`'s lifecycle lock; request
/// paths use [`begin_live`]. Every pinned transaction in flags-core goes
/// through one of the two (`tests/db.rs` scans for a bare `db.begin(`).
pub(crate) async fn begin_pinned(db: &Db, org: OrgId) -> Result<Tx<'static>> {
    let mut tx = db.begin(org).await?;
    sqlx::query("SELECT set_config('search_path', $1::text, true)")
        .bind(crate::unpinned::SEARCH_PATH)
        .execute(tx.conn())
        .await?;
    Ok(tx)
}
```

Replace `db.begin((*org_id).into())` (team.deleted), and `db.begin(org)` in `org_deleted` and
`member_removed`, with `begin_pinned(db, …)`. In `audit_purge.rs` `check`, replace
`db.begin(OrgId::new()).await?` with `crate::platform_events::begin_pinned(db, OrgId::new()).await?`
(both return `crate::error::Result`, so `?` is unchanged).

Unit tests: in `unpinned.rs` tests add

```rust
    /// Every unpinned transaction resolves names in `public` only (#97).
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_pins_the_search_path(pool: PgPool) {
        let db = Db::from_pool(pool);
        for maintenance in [false, true] {
            let mut tx = begin_as_tenant_role(&db, maintenance).await.unwrap();
            let path: String = sqlx::query_scalar("SELECT current_setting('search_path')")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(path, SEARCH_PATH, "maintenance: {maintenance}");
        }
    }
```

In `maintenance.rs` `the_role_and_marker_end_with_the_transaction`, also read
`current_setting('search_path')` before the loop on the one-connection pool and assert it is the
same after each commit/rollback (rename the test to
`the_role_marker_and_search_path_end_with_the_transaction`). This one passes before the fix too:
it is the no-leak check, not one of the red-first tests.

Also in `unpinned.rs` tests, cover the fallback role: in
`begin_keeps_a_non_exempt_role_that_cannot_assume_the_tenant_role`, before the rollback, assert
`SELECT current_setting('search_path')` inside `tx` equals `SEARCH_PATH`.

- [x] **Step 4: Run and see them pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15448/otto_flags cargo test -p flags-core`
Expected: all pass, including the four shadow tests and the two unit tests; none skipped.
Then `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`
(fix anything they report before committing).

- [x] **Step 5: Commit**

```bash
git add crates/flags-core
git commit -m "fix(core): resolve names in public only in every flags-core transaction" \
  -m "A schema named after the tenant role, first on the default search_path, could redirect any unqualified table name. Each transaction now sets search_path transaction-locally, folded into the statement it already runs, so the SDK key, request, and maintenance paths gain no round trip." \
  -m "Fixes #97"
```

### Task 2: Guard pinned transactions with a source scan

**Files:**
- Modify: `crates/flags-core/tests/db.rs` (`no_raw_pool_queries_outside_the_allowed_sites` and a
  new test beside it)

**Interfaces:** consumes `begin_pinned` and `begin_live` from Task 1.

**Out-of-band / invariants:** none.

**Model:** sonnet — mechanical refactor of an existing test.

- [x] **Step 1: Write the test** — factor the directory walk and per-file counting of
  `no_raw_pool_queries_outside_the_allowed_sites` (everything from `let crates = …` through
  building `found`) into `fn count_in_crate_src(patterns: &[&str]) -> BTreeMap<String, usize>`
  (keep the test-module tail check inside it), make the existing test call it with its
  `PATTERNS` (behavior unchanged), and add:

```rust
/// Every pinned transaction in the crates' source opens through
/// `platform_events::begin_live` or `platform_events::begin_pinned`, which pin
/// the search path (#97). The scan matches the text `db.begin(`: every `Db` in
/// the workspace is held as `db` (or `self.db`, `state.db`); one under another
/// name would escape it.
#[test]
fn no_pinned_begin_outside_the_search_path_helpers() {
    let found = count_in_crate_src(&["db.begin("]);
    let found: BTreeMap<&str, usize> = found.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    assert_eq!(
        found,
        BTreeMap::from([("flags-core/src/platform_events.rs", 2)]),
        "open pinned transactions with `begin_live` or `begin_pinned`"
    );
}
```

- [x] **Step 2: Prove it bites** — temporarily revert one webhook site to `db.begin(org)`, run
  `DATABASE_URL=postgres://flags:flags@localhost:15448/otto_flags cargo test -p flags-core --test db no_pinned_begin` and see it fail, then restore.

- [x] **Step 3: Run and see it pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15448/otto_flags cargo test -p flags-core --test db no_`
Expected: both scan tests pass.

- [x] **Step 4: Full gates** — test, lint, format, extra, and `podman build -t otto-flags:check .`
  (Global Constraints), all green.

- [x] **Step 5: Commit**

```bash
git add crates/flags-core/tests/db.rs
git commit -m "test(core): refuse a pinned transaction that skips the search_path helpers" \
  -m "A new bare db.begin would open a transaction with the role's default search_path again. The scan keeps every pinned transaction on begin_live or begin_pinned." \
  -m "Fixes #97"
```
