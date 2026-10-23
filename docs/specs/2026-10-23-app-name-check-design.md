# Enforce the app name rule with a database CHECK constraint

**Status:** IMPLEMENTED (#172)
**Ticket:** savvagent/otto-flags#157 (follow-up to #153)

## Brief

> `sdk_guide` returns a guide with the app's name substituted into Markdown an agent reads as
> instructions. #153 re-validates the name at render time (`validate_name`) [...]. The database
> itself does not enforce the same rule: `flag_apps.name` has no CHECK constraint, so a future
> write path that skips `validate_name` (a rename tool, a backfill) could store text that other
> consumers also trust.
>
> Add a migration with `CHECK (name ~ '^[A-Za-z0-9._-]{1,64}$') NOT VALID`, then `VALIDATE
> CONSTRAINT` only after confirming existing rows (production diagnostics are SELECT-only and run
> by Rob). [...] A new migration file only: never edit an applied one.

Acceptance: a new `NNNN_*.sql` adds the constraint (contiguity test and `migrations-append-only`
pass); a `flags-core` db test shows an invalid name is rejected at the database; Rob confirms
existing rows pass before `VALIDATE`.

## Premise corrections

None. `apps.rs:138` (`create_app`) is the only app-name write path and calls `validate_name`.

## Assumptions

- Migration number is 0009 (0001..0008 exist).
- The `VALIDATE CONSTRAINT` step is a separate, later migration, not part of this PR: the ticket
  gates it on a human-only production SELECT. This PR therefore ships the NOT VALID constraint only.
- The constraint is named `flag_apps_name_check` (Postgres's default for a column-less table CHECK
  would be `flag_apps_check`; the explicit name makes the test and error text unambiguous).
- Regex `$` in a Postgres ARE matches only at end of string, so a trailing newline is refused; the
  test pins this.

## Goal & Success Criteria

The database refuses any `flag_apps.name` that `validate_name` would refuse, without risk to the
rollout.

- New inserts and updates with an invalid name fail with `flag_apps_name_check`.
- Existing rows are untouched; the migration cannot fail on legacy data.
- `cargo test --workspace` passes, including the contiguity test.

## Scope

In: migration 0009, one db test. Out: renaming apps, changing `validate_name`, `VALIDATE CONSTRAINT`.

## Architecture

`crates/flags-core/migrations/0009_flag_apps_name_check.sql`: `ALTER TABLE flag_apps ADD CONSTRAINT
flag_apps_name_check CHECK (name ~ '^[A-Za-z0-9._-]{1,64}$') NOT VALID`. No Rust change, no public
interface change.

## Invariants impact

- Append-only migrations: new file only.
- Rollout/rollback compatibility: additive. The previous release writes only names passing
  `validate_name` (same character set and 1-64 length), so none of its writes trips the constraint.
  `validate_name` counts bytes (`name.len()`), the regex counts characters; for the ASCII-only set
  they coincide.
- Tenant isolation: no new table; no RLS change.

## Migration

See Architecture. `ADD CONSTRAINT ... NOT VALID` takes a brief ACCESS EXCLUSIVE lock without
scanning the table; it waits behind any open transaction on `flag_apps` and readers queue behind
it, so the file starts with `SET LOCAL lock_timeout = '3s'` like 0002, 0005, 0007 and 0008.

## Out-of-band

Human (Rob), after merge, before any VALIDATE migration: production SELECT-only diagnostic

    SELECT id, org_id, name FROM flag_apps WHERE name !~ '^[A-Za-z0-9._-]{1,64}$';

Zero rows means a follow-up migration may `VALIDATE CONSTRAINT flag_apps_name_check`. Nonzero rows
need a decision from Rob. Not a merge blocker: the NOT VALID constraint is safe regardless.

## Error Handling & Edge Cases

Empty, 65 characters, whitespace, newline (embedded and trailing), `;` and non-ASCII are refused;
64 characters, digits-only, and mixed `._-` are accepted. A violating write surfaces as a database
error; `create_app` already rejects these earlier, so no new user-visible error path.

## Testing Approach

`crates/flags-core/tests/db.rs`: `the_database_rejects_an_app_name_validate_name_would_reject`
inserts raw rows (bypassing `validate_name`) and asserts the constraint name in the error, plus an
UPDATE case. Gates: the section's test, lint, format, extra, and the Docker build.

## Risks & Open Questions

Legacy production rows that violate the rule are not expected (`create_app` has always validated).
If the diagnostic above finds one, it stays readable, but Postgres checks a CHECK constraint on every
UPDATE of the row, NOT VALID or not, whichever column changes: `set_app_environments` and key
rotation would fail for that app until it is renamed, in this release and after a rollback. Rob
should rename such an app (or decide otherwise) before anything depends on those tools for it.
