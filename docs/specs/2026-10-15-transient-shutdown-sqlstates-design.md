# Treat Postgres shutdown and startup SQLSTATEs as transient

**Status:** DRAFT
**Issue:** #56

## 1. Brief

From #56 (found while reviewing #55, for #43):

> `Error::is_transient` (`crates/flags-core/src/error.rs`, `TRANSIENT_SQLSTATES`) treats only
> `40001`, `40P01`, `55P03`, and `57014` as transient. Since #55, `57P01` (`admin_shutdown`, which
> is what a Fly Postgres restart looks like) and `57P03` (`cannot_connect_now`, a database still
> starting up) are logged by name, but background work still classes them as permanent failures.
> Those are the failures most likely to pass on their own a few seconds later.
>
> `is_transient` decides whether background jobs keep retrying. Its doc says a scheduled retry
> "must not keep retrying a constraint violation or a missing table". `57P01` and `57P03` are
> neither.
>
> **Acceptance criteria**
> - [ ] Decide whether `57P01` and `57P03` (and possibly `57P02` `crash_shutdown`) belong in
>   `TRANSIENT_SQLSTATES`, and record the reasoning in its doc comment.
> - [ ] If yes, the `only_contention_cancellation_and_connection_failures_are_transient` unit test
>   covers them.
> - [ ] `retriable` (what an agent sees) is unchanged.
>
> **Out of scope:** the reason names and log fields (#43).

## 2. Premise corrections

None. The brief matches the code: `TRANSIENT_SQLSTATES` (`error.rs`) holds the four codes named,
and its only consumer is `rollback.rs` (`classify` and `PolicyFailure::transient`), which cools a
flag down for `FAILURE_COOLDOWN` (an hour) on a non-transient failure.

## 3. Decision

**Yes: `57P01`, `57P02`, and `57P03` are transient.** All three say the server, not the
statement, was the problem, and each clears once the server is back up, which on Fly takes
seconds:

- `57P01` `admin_shutdown`: the server was shut down or restarted (a Fly Postgres restart or
  failover), or an operator terminated the backend (`pg_terminate_backend`). The statement did
  nothing wrong; a new connection after the restart runs it.
- `57P02` `crash_shutdown`: another backend crashed and the postmaster reset every session while
  it recovers. Same shape as `57P01`, from a crash instead of a request.
- `57P03` `cannot_connect_now`: the server is starting up, in recovery, or shutting down and not
  yet accepting connections. It is the error a restart leaves behind for the next few seconds.

Retrying these after the usual debounce costs one more attempt per due flag; classing them as
permanent silences auto-rollback for that flag for an hour after a routine database restart, when
the checks are most needed (the same reasoning that already makes a pool timeout transient).

**Not added:** `57P04` `database_dropped` (the database is gone: retrying cannot fix it), `57000`
and any other `57xxx` (`operator_intervention`, too broad to call self-healing), and the `08xxx`
connection exceptions (sqlx reports a dropped connection as `sqlx::Error::Io`, already transient;
an `08xxx` Postgres sends, such as `08P01` `protocol_violation`, is not self-healing). `57014`
stays, as before.

## 4. Assumptions

- The three codes go in the one list, `TRANSIENT_SQLSTATES`, rather than a separate one: they
  share its meaning ("a later attempt can avoid it"), and `db_is_transient` is unchanged.
- The `rollback.rs` module doc's parenthetical list of transient failures gains "a database
  restart", so it does not read as exhaustive and wrong. Doc only (one parenthetical, reflowed); no code there changes.
- Run Profile: every value comes from `CLAUDE.md` > Development Workflow; nothing assumed. Tests
  run against a dedicated local Postgres on port 15442 (the parallel-agent protocol), not 15434.

## 5. Goal & success criteria

A Postgres restart no longer makes the auto-rollback checker give up on a flag for an hour.

- `Error::Db` and `Error::Tenant(otto_tenant::Error::Db)` with SQLSTATE `57P01`, `57P02`, or
  `57P03` are `is_transient()`.
- `57P04` and `57000` stay not transient (asserted).
- `retriable()` is unchanged: no edit to it; its existing test still passes.
- The `TRANSIENT_SQLSTATES` doc comment records the decision and the exclusions.

## 6. Scope

In: `crates/flags-core/src/error.rs` (`TRANSIENT_SQLSTATES`, its doc comment, the `is_transient`
doc comment, the unit test); the module doc's transient-failure parenthetical in `crates/flags-core/src/rollback.rs`.

Out: reason names and log fields (#43); `retriable`; the `08xxx` class; any change to how the
checker schedules retries.

## 7. Architecture

`error.rs`:

```rust
const TRANSIENT_SQLSTATES: &[&str] = &[
    "40001", "40P01", "55P03", "57014", "57P01", "57P02", "57P03",
];
```

with the doc comment carrying section 3's reasoning, and `is_transient`'s doc naming "the
database shutting down, restarting, or still starting up" among its kinds.

No public-interface change: `TRANSIENT_SQLSTATES` is private; `is_transient` keeps its signature
and returns `true` for more inputs. `PolicyFailure::transient` and `Checked::Retry` follow.

## 8. Invariants impact

- **Database text never reaches a tool response:** untouched; only codes are compared.
- **The database is small and shared:** a restart's failed checks now retry after the debounce
  (one check per due flag, flags one at a time) instead of cooling down. The checker already does
  this for pool timeouts; no new fan-out.
- No migration, no SQL, no SDK or MCP contract change. `retriable` (the MCP `retriable` field)
  is unchanged.

## 9. Out-of-band

None.

## 10. Error handling & edge cases

- A backend terminated by an operator to stop a runaway check also reports `57P01`; the check is
  bounded (2 s lock wait, 10 s statements, 15 s total), so a retry after the debounce is the
  same bounded work.
- After `57P01` the connection is dead; sqlx discards it, so the retry gets a fresh connection.
- sqlx's pool already treats `57P03` as transient while connecting
  (`PgDatabaseError::is_transient_in_connect_phase`, sqlx 0.8.6): it retries the connect until
  the acquire timeout and then reports `PoolTimedOut`, already transient here. A `57P03` reaches
  `is_transient` only from a path outside that loop (a direct connect); classing it transient
  agrees with sqlx's own judgement.

## 11. Testing approach

Unit test `only_contention_cancellation_and_connection_failures_are_transient` (`error.rs`):
`57P01`, `57P02`, `57P03` (bare and wrapped in `otto_tenant::Error::Db`) in the transient list;
`57P04` and `57000` in the permanent list. `retriable_keeps_its_meaning_for_mcp_callers` asserts
`57P01` is retriable too. Gates: the section's test, lint, format, extra, and podman build.

## 12. Risks & open questions

- A persistently unavailable database now retries every debounce instead of cooling down. That is
  already true today, since a database that will not accept connections surfaces as `PoolTimedOut`
  or `Io`, both transient; the checks fail at connect time and hold no connection.
