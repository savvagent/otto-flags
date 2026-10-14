# Failure reasons: SQLSTATE and transport sub-kind above debug

**Status:** IMPLEMENTED (#55)
**Issue:** #43

## 1. Brief

From #43 (found reviewing #42 / #35):

> #29 and #35 log a text-free reason above `debug` and keep the error text at `debug`. Some failure
> classes now share one catch-all reason, so at `info` an operator can't tell them apart:
>
> - `db_failure_reason` (`crates/flags-core/src/error.rs`) maps any SQLSTATE outside its list to
>   `database_error`. That includes `57P01` (admin shutdown, which is what a Fly Postgres restart
>   looks like), `57P03`, `25006`, and the `22xxx` data exceptions. It maps any other sqlx variant
>   to `other`.
> - `platform_failure_reason` maps every `Transport` error to `transport`: DNS failure, connection
>   refused, TLS, and timeout look the same.
>
> The SQLSTATE itself is a five-character code with no data in it, so logging it is safe.
>
> **Acceptance criteria**
> - [ ] Database failures above `debug` also carry a `sqlstate` field when Postgres raised the
>   error. Reasons stay as they are.
> - [ ] Optionally, `57`-class and `25`-class conditions get names.
> - [ ] Transport failures carry a sub-kind (`connect`, `timeout`, or `other`), where `reqwest`
>   exposes it without its text.
> - [ ] Every site that logs a reason, `flags-mcp` included, gets the new fields from the one
>   definition in `flags-core`.
>
> **Out of scope:** changing which text is logged at `debug`.

## 2. Premise corrections

- **"Reasons stay as they are" and "57/25 get names" overlap.** Naming the 57 and 25 classes
  changes the reason for those SQLSTATEs from `database_error` to a name. Every reason that is
  already a name keeps it (`query_canceled` for `57014` included); only codes that today fall into
  the `database_error` catch-all get new names. That is what the optional AC asks for.
- **The SQLSTATE is attacker-irrelevant but not type-guaranteed.** `DatabaseError::code()` returns
  a `Cow<str>`; for Postgres it is always five characters, but nothing in the type says so. The
  field is only logged after checking it is exactly five ASCII uppercase letters or digits, so no
  other text can ride along in it.
- **Not every site that logs a database failure logs a reason.** `crates/flags-core/src/rollback.rs`
  logs only `code` (always `internal_error` for a database failure) at `:1162` and `:1682`. The AC
  covers sites that log a reason; giving the rollback checker reasons is a separate change, filed
  by the agent before this PR as #54 rather than widened into this one.
- **`usage_outbox.last_error`** stores `<reason>` or `<reason> <status>` (#44). That column is not
  a log line; it keeps its current format (no transport sub-kind), so its contents do not change.

## 3. Assumptions

- **One type carries the fields.** `flags_core::error::Failure` (a `Copy` struct: `reason`,
  `status`, `sqlstate`, `transport`) replaces the `(&'static str, Option<u16>)` tuple and the bare
  `&'static str` that `failure_reason`, `platform_failure_reason`, `db_failure_reason`, and
  `ShipError::failure_reason` return today. Every log site reads its fields from it. All callers
  are inside this workspace; no public wire or SDK interface changes.
- **Log field names:** `sqlstate` (e.g. `sqlstate="57P01"`) and `transport` (e.g.
  `transport="connect"`). Absent (`None`) fields are omitted from the line, as `status` already is.
- **Which sites emit which fields:** a site holding a `flags_core::Error` (which can be either kind)
  emits all four; a site holding only a `sqlx::Error` emits `reason` and `sqlstate`; a site holding
  only an `otto_resource::Error` emits `reason`, `status`, and `transport`. The debug twin of each
  event carries the same fields as the line above it, plus the text it already carries (this adds
  `status` to the MCP helper's debug line, which omits it today).
- **Transport sub-kind order:** `reqwest::Error::is_connect()` → `connect` (connection refused,
  DNS, TLS handshake); else `is_timeout()` → `timeout`; else `other`. Both are boolean
  predicates; no text is read. `otto-resource` sets only a total request timeout (no
  `connect_timeout`), so a connect that hangs until that timeout (a blackholed platform) is
  reported by reqwest as a request timeout and logs `timeout`, not `connect`: `timeout` means "no
  answer in time", whether or not the connection opened.
- **57 and 25 names** (Postgres condition names, appendix A of the Postgres docs): `57P01`
  `admin_shutdown`, `57P02` `crash_shutdown`, `57P03` `cannot_connect_now`, any other `57xxx`
  (except `57014`, which stays `query_canceled`) `operator_intervention`; `25006`
  `read_only_sql_transaction`, `25P02` `in_failed_sql_transaction`, any other `25xxx`
  `invalid_transaction_state`. `22xxx` stays `database_error` with its `sqlstate` now logged
  (the issue does not ask for a name).
- **`is_transient` is unchanged.** Whether `57P01` should be retried by background jobs is a
  separate question; this change is about naming only.
- **Tests produce real transport errors** with `otto_resource::PlatformClient` against local
  sockets (a closed port, a listener that never answers with a short client timeout, a listener
  that answers with bytes that are not HTTP), so no `reqwest` dependency is added.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

An operator reading `info`-level logs can tell a Postgres restart from a data exception, and a
refused platform connection from a slow one, without any error text reaching that level.

- A database error raised by Postgres logs `sqlstate="<code>"` above debug at every site that logs
  a database failure reason; a non-Postgres sqlx error (pool timeout, I/O) logs no `sqlstate`.
- `57P01`, `57P03`, `25006` and the rest listed above log a named reason; every reason that was
  already a name is unchanged.
- A platform transport failure logs `reason="transport"` with `transport="connect"`, `"timeout"`,
  or `"other"`.
- `crates/flags-server/tests/log_text.rs` passes with no exceptions.
- No migration, no wire change, no new dependency.

## 5. Scope

**In:** `crates/flags-core/src/{error,usage,notify,entitlements}.rs`,
`crates/flags-mcp/src/{error,auth}.rs`, `crates/flags-api/src/{error,platform}.rs`,
`crates/flags-server/src/{main,health}.rs`, and the existing log tests
(`crates/flags-mcp/tests/failure_logs.rs`, `crates/flags-api/tests/failure_logs.rs`,
`crates/flags-server/tests/usage_logs.rs`).

**Out:** what is logged at `debug` (per the issue); `is_transient` / `retriable` semantics; the
`usage_outbox.last_error` format; the rollback checker's code-only logs (follow-up issue); naming
`22xxx` or any class beyond 57 and 25.

## 6. Architecture

`crates/flags-core/src/error.rs` (the one definition):

```rust
/// A text-free description of a failure for logs above debug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub reason: &'static str,
    pub status: Option<u16>,          // platform HTTP status
    pub sqlstate: Option<Sqlstate>,   // Postgres raised it
    pub transport: Option<&'static str>, // "connect" | "timeout" | "other"
}
impl Failure {
    pub const fn reason(reason: &'static str) -> Self; // all other fields None
    pub fn sqlstate(&self) -> Option<&str>;            // for a log field
}

/// A Postgres SQLSTATE: exactly five ASCII uppercase letters or digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sqlstate([u8; 5]);
impl Sqlstate {
    pub fn parse(code: &str) -> Option<Self>;
    pub fn as_str(&self) -> &str;
}
```

- `Error::failure_reason(&self) -> Failure`; `platform_failure_reason(&otto_resource::Error) ->
  Failure` (sets `status` for `Status`, `transport` for `Transport`);
  `db_failure_reason(&sqlx::Error) -> Failure` (sets `sqlstate` for `sqlx::Error::Database`
  whose code parses; reason mapping gains the 57/25 names, matched after the existing exact codes
  so `57014` stays `query_canceled`). A private `transport_kind(&otto_resource::Error) -> Option<&'static str>` holds
  the sub-kind rule, calling `is_connect()` / `is_timeout()` on the `Transport` variant's value;
  a signature naming `reqwest::Error` would need a `reqwest` dependency, so none does.
- `usage.rs`: `ShipError::failure_reason -> Failure`; `stored_reason` reads `.reason` and
  `.status` (output unchanged); the quota fail-open timeout uses `Failure::reason("timeout")`;
  the shipping-loop, final-flush, and reschedule-record events log the new fields.
- `entitlements.rs`: same, with `Failure::reason("timeout")` for the timeout arm.
- `notify.rs` (three DB sites), `flags-server/src/health.rs` (one DB site): `reason`, `sqlstate`.
- `flags-server/src/main.rs`: both sweep events (`flags_core::Error`: all four) and
  `check_platform` (platform: `reason`, `status`, `transport`).
- `flags-api/src/error.rs` (SDK request failed) and `flags-api/src/platform.rs` (apply failed):
  all four.
- `flags-mcp/src/error.rs`: `log_server_failure(what, code, failure: Failure, detail)` logs all
  four; callers in `error.rs` and `auth.rs` pass `Failure` values unchanged otherwise.

Log site shape (fields after any existing `code`/`org` fields, in this order):

```rust
let f = e.failure_reason();
tracing::warn!(code = e.code(), reason = f.reason, status = f.status,
    sqlstate = f.sqlstate(), transport = f.transport, "telemetry sweep failed");
```

No public HTTP/MCP/SDK interface changes. The Rust API change (return types) is internal to the
workspace and breaking only for in-workspace callers, all updated here.

## 7. Invariants impact

- **SDK-reported text / database error text never reaches a tool response:** unchanged; wire
  messages are not touched. The new log fields are a validated five-character code and fixed
  `&'static str` names.
- **Remote text above debug** (#29/#35 rule enforced by `log_text.rs`): holds; the new fields
  carry no text. `log_text.rs` keeps no exceptions.
- **Append-only migrations / no SQL outside flags-core / tenant isolation / metering:** not
  touched.

## 8. Migration

None.

## 9. Out-of-band

None. No config key, secret, scope, or registration.

## 10. Error handling and edge cases

- A `Database` error whose `code()` is `None` or not a valid SQLSTATE: reason as today
  (`database_error` or the matched name), no `sqlstate` field.
- `Error::Tenant(otto_tenant::Error::Db(e))` gets the same `sqlstate` as `Error::Db(e)`.
- A non-DB, non-platform `flags_core::Error`: `Failure::reason(self.code())`.
- A reqwest error that is both connect and timeout (an OS-level connect timeout): `connect`. A
  hang that the client's total timeout cuts off logs `timeout` (see Assumptions).

## 11. Testing approach

Gates per CLAUDE.md (test with `DATABASE_URL` on 15434, clippy, fmt, migrations-unchanged,
`podman build`).

- `crates/flags-core/src/error.rs` unit tests: the existing reason table compares
  `Failure` fields; new cases for the 57/25 names, `sqlstate` present for Postgres errors and
  absent for pool/I/O errors and for invalid codes (`Sqlstate::parse` rejects lowercase,
  wrong length, non-alphanumerics); transport sub-kinds from real `PlatformClient` failures
  (`#[tokio::test]`, local sockets only).
- `crates/flags-mcp/tests/failure_logs.rs` and `crates/flags-api/tests/failure_logs.rs`: a case
  with a `57P01` database error whose ERROR/WARN line carries `reason="admin_shutdown"
  sqlstate="57P01"`; the mcp test adds a `from_platform` transport case asserting
  `reason="transport" transport="connect"`.
- `crates/flags-server/tests/usage_logs.rs`: update the tuple assertions to `Failure` fields.
- `crates/flags-server/tests/log_text.rs`: unchanged, must pass.

## 12. Risks and open questions

- Dashboards or alerts matching `reason="database_error"` for a 57/25 code would stop matching;
  none exist in this repo (`docs/deploy/fly.md` does not reference reasons). Low risk.
- Field ordering in a test's expected substring: tests assert contiguous substrings, so the field
  order at each site matters; the order is fixed above.
