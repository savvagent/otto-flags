# Name the purge function by schema in the startup check's hints

**Status:** DRAFT
**Issue:** #98

## 1. Brief

Found by the security review of the #53 fix (#99). Quoted from #98 (untrusted ticket text):

> `PurgeReport::problems` and `summary` (`crates/flags-core/src/audit_purge.rs`) tell the operator
> to run `ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO …` and `GRANT EXECUTE ON FUNCTION
> purge_deleted_org_audit(uuid) TO …`, without a schema. In a session where another schema on the
> `search_path` holds a same-named function, the pasted statement would change that function, not
> the one the check inspects (`audit_purge::FUNCTION`, `public.…`).
>
> Separately, the "does not exist … run migrations (0004)" message is misleading when 0004 is
> recorded as applied but created the function outside `public`, because migrations will not
> re-run it.
>
> **Acceptance criteria**
> - The suggested statements name the function as `audit_purge::FUNCTION` does, built from the
>   same source.
> - The missing-function message covers the case where 0004 is applied but the function is not in
>   `public`.
> - Existing tests are updated only where they pin the message text.

## 2. Premise corrections

- `summary` suggests no statement; it only names the function. It is still changed to name it as
  `FUNCTION` does, so the boot log line names exactly the function the check inspected. The healthy
  boot line quoted in `docs/deploy/fly.md` changes with it and is updated in this PR.
- No existing test pins the text this change alters: the unit tests and
  `audit_purge_check_reports_a_missing_function` (`crates/flags-core/tests/db.rs`) match on
  `does not exist`, `OWNER TO`, `GRANT EXECUTE`, role names, and summary fragments, all of which
  survive. So no existing assertion is edited; new assertions are added.

## 3. Assumptions

- **The message alone covers the "applied but not in `public`" case**; the check does not look for
  the function in other schemas. Locating it would add a field to the public `PurgeReport` struct
  (a breaking change for every struct literal, including its doc test) for a state 0004 reaches
  only when the migrating role had a schema of its own name on its `search_path`. The message
  instead gives the operator a catalog query that finds it and the statement that moves it back.
- **The remedy for a function outside `public` is `ALTER FUNCTION <schema>.… SET SCHEMA public`.**
  0004 pins the function's own `search_path` and qualifies every table it touches, so moving it
  does not change what it does; owner and grants move with it. This assumes 0004's tables are in
  `public`, as `PURGE` (#53) and the function body already do; a deployment whose tables also
  landed elsewhere is out of scope. On Postgres 15+ only a superuser (or the owner holding CREATE
  on `public`) may move a function into `public`, so the message says "as a superuser". If the query finds nothing, the
  function was dropped, and the message says to recreate it from
  `crates/flags-core/migrations/0004_audit_purge.sql`, since migrations will not.
- **One source for the name.** The existing private `purge_function!()` macro is split into the
  unqualified name (`purge_name!()`, `"purge_deleted_org_audit"`) and the qualified name built from
  it (`concat!("public.", purge_name!())`), so `FUNCTION`, `PURGE`, the suggested statements, and
  the catalog query all derive from one literal. Messages use `FUNCTION` through format capture.
- All Run Profile values come from the CLAUDE.md section; none are assumed.

## 4. Goal and success criteria

Every statement or name the startup check gives the operator refers to the function the check
inspected, and the missing-function message is correct whether or not 0004 has run.

- `problems()` suggests `ALTER FUNCTION public.purge_deleted_org_audit(uuid) OWNER TO …` and
  `GRANT EXECUTE ON FUNCTION public.purge_deleted_org_audit(uuid) TO …`, built from `FUNCTION`.
- `summary()` reads `audit purge … : public.purge_deleted_org_audit(uuid) is owned by …`.
- The missing-function problem keeps `does not exist` and the run-migrations advice, and adds: if
  0004 is recorded as applied (version 4 in `_sqlx_migrations`), find the function (catalog
  query) and, as a superuser, move it into `public`
  (`ALTER FUNCTION <schema>.purge_deleted_org_audit(uuid) SET SCHEMA public`, quoting `<schema>`
  if it needs it), or recreate it from 0004's file if it is gone.
- A DB test proves the advice: with the function moved to another schema, the check reports it
  missing with that advice, and after the suggested `SET SCHEMA public` the check passes.
- Every existing test passes unchanged. No migration.

## 5. Scope

In: `PurgeReport::problems` and `summary` text, the private name macros in `audit_purge.rs`, new
unit tests beside the code, one DB test in `crates/flags-core/tests/db.rs`, and the two quoted
lines in `docs/deploy/fly.md`.

Out: what `inspect` reads or how `check` decides; `PurgeReport`'s fields; the `PURGE` call; the
unqualified statements inside existing tests; historical specs and plans.

## 6. Architecture

`crates/flags-core/src/audit_purge.rs`:

```rust
macro_rules! purge_name { () => { "purge_deleted_org_audit" }; }
macro_rules! purge_function { () => { concat!("public.", purge_name!()) }; }

pub const FUNCTION: &str = concat!(purge_function!(), "(uuid)");           // value unchanged
pub(crate) const PURGE: &str = concat!("SELECT ", purge_function!(), "($1)"); // unchanged
```

`problems()` and `summary()` interpolate `{FUNCTION}` wherever they named
`purge_deleted_org_audit(uuid)`. The missing-function message names the bare function with
`purge_name!()` (in the catalog query's `proname = '…'` and in `<schema>.…(uuid) SET SCHEMA
public`). No public signature or type changes; `FUNCTION` and `PURGE` keep their values, so
`inspect`, `check`, and the purge call behave exactly as before.

`docs/deploy/fly.md` (Infrastructure, the migrating-role bullet): the quoted healthy boot line and
the `ALTER FUNCTION … OWNER TO …` example gain `public.`.

## 7. Invariants impact

- No SQL outside `flags-core`, runtime queries only: unchanged (no new query in the server; the
  suggested statements are text).
- Append-only migrations: no migration.
- Tenant isolation, RLS under `otto_app`: unchanged; the check's logic is untouched.
- Database error text never reaches a tool response: unaffected (boot log only).

## 8. Migration

None.

## 9. Out-of-band

None. The statements are advice for a human; on production they remain human-only (Rob), and this
change runs none of them. No config, secret, scope, or recurring-cost impact.

## 10. Error handling and edge cases

- Function in another schema: `to_regprocedure('public.…(uuid)')` is NULL, so `owner` is `None`,
  the missing-function message applies, and the new text tells the operator how to find and move it.
- Function dropped after 0004 ran: the catalog query returns nothing; the message says to recreate
  it from 0004's file.
- Role names in suggested statements stay quoted with `quote_ident`; `public` and the function name
  are fixed literals that need no quoting.

## 11. Testing approach

- Unit tests in `audit_purge.rs`: the owner and grant problems contain
  `ALTER FUNCTION {FUNCTION} OWNER TO` and `GRANT EXECUTE ON FUNCTION {FUNCTION} TO`, and
  `FUNCTION` starts with `public.`; the missing-function problem names `FUNCTION`, says what to do
  when 0004 is recorded as applied, and gives the `SET SCHEMA public` statement; the summary names
  `FUNCTION`.
- DB test `audit_purge_check_reports_a_function_outside_public` in `crates/flags-core/tests/db.rs`
  (beside `audit_purge_check_reports_a_missing_function`): in one rolled-back transaction, create
  a schema, move the function into it, `SET LOCAL ROLE otto_app`, inspect (owner `None`, the
  problem gives the `SET SCHEMA public` advice); `RESET ROLE`, run the advised statement with the
  schema filled in, `SET LOCAL ROLE otto_app`, inspect again (no problems).
- Gates: test with `DATABASE_URL` on the dedicated port, clippy, fmt, the migrations extra check,
  and `podman build` (`crates/**`).

## 12. Risks and open questions

- The boot log line changes text. `docs/deploy/fly.md` quotes it and is updated; the Ship > verify
  log markers in CLAUDE.md do not quote it.
