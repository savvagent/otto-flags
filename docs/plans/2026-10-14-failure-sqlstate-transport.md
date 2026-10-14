# Failure SQLSTATE and Transport Sub-kind Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Every log site that writes a text-free failure reason above `debug` also writes the
Postgres `sqlstate` and the platform `transport` sub-kind, from one `Failure` type in
`flags-core`, and 57/25-class SQLSTATEs get names.

**Architecture:** `crates/flags-core/src/error.rs` gains `Failure` and `Sqlstate`; the three
reason functions and `ShipError::failure_reason` return `Failure`. Each log site binds
`let f = ….failure_reason();` and logs `reason = f.reason, status = f.status, sqlstate =
f.sqlstate(), transport = f.transport` (the subset that applies to its error type). The MCP
helper `log_server_failure` takes a `Failure`.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, tracing, otto-resource (reqwest 0.12).

**Ticket:** savvagent/otto-flags#43

**Spec:** `docs/specs/2026-10-14-failure-sqlstate-transport-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/feat-43-failure-sqlstate-transport`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434. Only one DB test run
  at a time against that Postgres.
- Invariants that apply: error text never logged above `debug` (`log_text.rs`, which keeps no
  exceptions); database error text never reaches a tool or SDK response (wire messages are not
  touched); no migration; no new dependency (no `reqwest` in any `Cargo.toml`); no SQL change.
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #43`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/error.rs` | `Failure`, `Sqlstate`, reason functions, 57/25 names, transport sub-kind, unit tests |
| **Modify.** `crates/flags-core/src/usage.rs` | `ShipError::failure_reason -> Failure`; log sites; `stored_reason`; unit test |
| **Modify.** `crates/flags-core/src/notify.rs` | three DB log sites |
| **Modify.** `crates/flags-core/src/entitlements.rs` | one platform log site |
| **Modify.** `crates/flags-server/src/health.rs` | one DB log site |
| **Modify.** `crates/flags-server/src/main.rs` | two sweep sites, `check_platform` |
| **Modify.** `crates/flags-api/src/error.rs` | SDK request failed site |
| **Modify.** `crates/flags-api/src/platform.rs` | apply-failed site |
| **Modify.** `crates/flags-mcp/src/error.rs` | `log_server_failure` takes `Failure` |
| **Modify.** `crates/flags-mcp/src/auth.rs` | two callers of `log_server_failure` |
| **Modify.** `crates/flags-server/tests/usage_logs.rs` | tuple assertions → `Failure` fields |
| **Modify.** `crates/flags-mcp/tests/failure_logs.rs` | sqlstate and transport cases |
| **Modify.** `crates/flags-api/tests/failure_logs.rs` | sqlstate case |

## Task Order & Rationale

Task 1 changes the return types, so every caller must change in the same task for the workspace
to compile; its tests are the `flags-core` unit tests. Task 2 adds the log-capture tests that
prove the fields reach the MCP and SDK log lines (they would pass only after Task 1; they are
written first within Task 2 against the new fields and must fail on a build where a site drops a
field — verified by temporarily removing `sqlstate = f.sqlstate()` from one site).

---

### Task 1: `Failure` in flags-core and every reason log site

**Files:**
- Modify: `crates/flags-core/src/error.rs`
- Modify: `crates/flags-core/src/usage.rs`, `crates/flags-core/src/notify.rs`, `crates/flags-core/src/entitlements.rs`
- Modify: `crates/flags-server/src/health.rs`, `crates/flags-server/src/main.rs`
- Modify: `crates/flags-api/src/error.rs`, `crates/flags-api/src/platform.rs`
- Modify: `crates/flags-mcp/src/error.rs`, `crates/flags-mcp/src/auth.rs`
- Modify: `crates/flags-server/tests/usage_logs.rs`
- Test: unit tests in `crates/flags-core/src/error.rs` and `crates/flags-core/src/usage.rs`

**Interfaces:** produces `flags_core::error::{Failure, Sqlstate}`,
`Error::failure_reason(&self) -> Failure`, `platform_failure_reason(&otto_resource::Error) ->
Failure`, `db_failure_reason(&sqlx::Error) -> Failure`, `ShipError::failure_reason(&self) ->
Failure`; Task 2 relies on the log field names `reason`, `status`, `sqlstate`, `transport`.

**Out-of-band / invariants:** none out-of-band. No error text above debug; no `reqwest` dependency.

**Model:** sonnet — mechanical across many sites, with a small, fully specified type.

- [x] **Step 1: Write the failing unit tests** in `crates/flags-core/src/error.rs` `mod tests`:

  - First rename the existing test double `struct Sqlstate(&'static str)` in `mod tests` to
    `FakeDbError` (and its uses in `db()`), so the new `super::Sqlstate` is not shadowed.

  - Change `failure_reasons_name_the_kind_without_the_text` to compare `e.failure_reason().reason`
    and `.status` (keep every existing row and its reason), and `platform_failure_reason(&e).reason`.
  - Add `sqlstate_is_logged_only_when_postgres_raised_it`:
    `Error::Db(db("57P01")).failure_reason()` has `reason == "admin_shutdown"`,
    `sqlstate() == Some("57P01")`, `transport == None`; `Error::Tenant(otto_tenant::Error::Db(db("22P02")))`
    has `reason == "database_error"`, `sqlstate() == Some("22P02")`;
    `Error::Db(sqlx::Error::PoolTimedOut)` has `sqlstate() == None`;
    `db_failure_reason(&db("bad code"))` and `db("57p01")` (lowercase) have `sqlstate() == None`.
  - Add `class_57_and_25_conditions_are_named` over `db_failure_reason(&db(code)).reason`:
    `57P01 admin_shutdown`, `57P02 crash_shutdown`, `57P03 cannot_connect_now`,
    `57000 operator_intervention`, `57014 query_canceled` (unchanged), `25006
    read_only_sql_transaction`, `25P02 in_failed_sql_transaction`, `25001
    invalid_transaction_state`.
  - Add `Sqlstate::parse` cases: `"40P01"` ok and `as_str()` round-trips; `""`, `"4000"`,
    `"400011"`, `"40p01"`, `"40 01"`, `"4000-"`, `"4000é"` rejected.
  - Add `#[tokio::test] async fn transport_failures_carry_a_sub_kind()`, using real
    `otto_resource::PlatformClient` calls (`usage_status(uuid::Uuid::nil())`) built with
    `ClientConfig::new(base, "https://flags.test/mcp", "secret")` and `timeout` overridden:
    - `connect`: bind `std::net::TcpListener` on `127.0.0.1:0`, take its port, drop it; base
      `http://127.0.0.1:<port>`; expect `reason == "transport"`, `transport == Some("connect")`.
    - `timeout`: bind a `tokio::net::TcpListener` on `127.0.0.1:0`, spawn a task that accepts and
      holds each socket open without writing (keep them in a `Vec` until the test ends); client
      timeout 200 ms; expect `transport == Some("timeout")`.
    - `other`: a listener task that accepts, reads once, writes `b"NOT HTTP\r\n\r\n"`, and
      closes (a response hyper cannot parse: deterministic, unlike a bare close); expect
      `transport == Some("other")`.
    Each assertion message prints the variant (`{e:?}`) so a wrong classification is diagnosable.
    `PlatformClient::new(cfg)` returns `Result`; `.expect("client")`.
    If `usage_status` maps the unparseable response to something other than `Transport`, print
    it and report NEEDS_CONTEXT instead of changing the expectation.

- [x] **Step 2: Run to verify they fail**
  Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --lib error::`
  Expected: compile errors (`Failure`, `Sqlstate`, `.reason` on a tuple).

- [x] **Step 3: Implement in `crates/flags-core/src/error.rs`**

```rust
/// A text-free description of a failure, for logs above debug (#29, #35, #43).
///
/// Every field is either a name this crate chose or a value with no room for
/// text: an HTTP status, a validated five-character SQLSTATE. Database and
/// platform text can carry SQL, schema names, response bodies, and URLs, so it
/// is logged only at debug. Log sites write the fields that apply to the error
/// they hold as `reason`, `status`, `sqlstate`, and `transport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    /// A fixed name for the kind of failure.
    pub reason: &'static str,
    /// The HTTP status the platform answered with.
    pub status: Option<u16>,
    /// The SQLSTATE, when Postgres raised the error.
    pub sqlstate: Option<Sqlstate>,
    /// For a platform transport failure: `connect`, `timeout`, or `other`.
    pub transport: Option<&'static str>,
}

impl Failure {
    /// A failure known only by its reason.
    pub const fn reason(reason: &'static str) -> Self {
        Self { reason, status: None, sqlstate: None, transport: None }
    }

    /// The SQLSTATE as a log field value.
    pub fn sqlstate(&self) -> Option<&str> {
        self.sqlstate.as_ref().map(Sqlstate::as_str)
    }
}

/// A Postgres SQLSTATE: five ASCII uppercase letters or digits, checked on
/// construction so nothing but the code can reach a log line through it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sqlstate([u8; 5]);

impl Sqlstate {
    pub fn parse(code: &str) -> Option<Self> {
        let bytes: [u8; 5] = code.as_bytes().try_into().ok()?;
        bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            .then_some(Self(bytes))
    }

    pub fn as_str(&self) -> &str {
        // Checked ASCII on construction.
        std::str::from_utf8(&self.0).expect("ascii sqlstate")
    }
}

impl std::fmt::Debug for Sqlstate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sqlstate({})", self.as_str())
    }
}
```

  `Error::failure_reason(&self) -> Failure`:
  `Db(e) | Tenant(Db(e)) => db_failure_reason(e)`, `Platform(p) => platform_failure_reason(p)`,
  `_ => Failure::reason(self.code())`. Update its doc comment to name the new fields.

  `platform_failure_reason(e) -> Failure`: `Status { status, .. }` → `Failure { status:
  Some(*status), ..Failure::reason("http_status") }`; `Transport(_)` → `Failure { transport:
  transport_kind(e), ..Failure::reason("transport") }`; the rest `Failure::reason(<same
  name as today>)`.

  Transport sub-kind: match on the `otto_resource::Error::Transport(t)`
  binding and call `t.is_connect()` / `t.is_timeout()` directly in a private
  `fn transport_kind(e: &otto_resource::Error) -> Option<&'static str>` — `is_connect()` →
  `"connect"`, else `is_timeout()` → `"timeout"`, else `"other"`; `None` for non-`Transport`.
  Method calls on the `reqwest::Error` value need no `reqwest` import or dependency.

  `db_failure_reason(e) -> Failure`: the existing mapping becomes the private
  `fn db_reason(e: &sqlx::Error) -> &'static str`, extended in the `Database` arm with, after the
  existing exact codes (`57014` stays first) and before the existing class prefixes:
  `57P01 admin_shutdown`, `57P02 crash_shutdown`, `57P03 cannot_connect_now`,
  `25006 read_only_sql_transaction`, `25P02 in_failed_sql_transaction`, then class prefixes
  `57 operator_intervention` and `25 invalid_transaction_state` alongside the existing `23`/`08`/`53`.
  `db_failure_reason` returns `Failure { sqlstate, ..Failure::reason(db_reason(e)) }` where
  `sqlstate` is `Sqlstate::parse(&code)` for `sqlx::Error::Database(db)` with `db.code()` `Some`,
  else `None`. Update its doc comment (it now returns the SQLSTATE too).

- [x] **Step 4: Update every caller** (field order at each event: existing leading fields such
  as `org`/`code`, then `reason, status, sqlstate, transport`, then the rest; debug twins carry
  the same fields plus their existing `error = %e`):
  - `crates/flags-core/src/usage.rs`: `ShipError::failure_reason -> Failure` (`Platform(p) =>
    platform_failure_reason(p)`, `Db(e) => db_failure_reason(e)`); quota fail-open:
    `failure.as_ref().map_or(Failure::reason("timeout"), platform_failure_reason)` then log
    `reason, status, transport`; reschedule-record site (`db_failure_reason`): `reason, sqlstate`;
    shipping loop and final flush: all four; `stored_reason`: match on `(f.reason, f.status)` —
    output unchanged; unit test at the bottom of the file comparing `failure_reason()` →
    compare `.reason`/`.status`.
  - `crates/flags-core/src/entitlements.rs`: the match yields a `Failure`
    (`Failure::reason("timeout")` for the elapsed arm); log `reason, status, transport`.
  - `crates/flags-core/src/notify.rs` (three sites) and `crates/flags-server/src/health.rs`:
    `let f = db_failure_reason(&e);` log `reason = f.reason, sqlstate = f.sqlstate()`.
  - `crates/flags-server/src/main.rs`: both sweep events all four; `check_platform`
    `reason, status, transport`.
  - `crates/flags-api/src/error.rs` and `crates/flags-api/src/platform.rs` (apply failure): all four.
  - `crates/flags-mcp/src/error.rs`: `log_server_failure(what, code, f: Failure, detail)` logs
    `code, reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport =
    f.transport` at error and the same plus `error = %detail` at debug (the debug line gains
    `status`, which it omits today); update its doc comment.
    Callers in `error.rs` and `crates/flags-mcp/src/auth.rs` already pass the function results;
    they compile unchanged once the types line up.
  - `crates/flags-server/tests/usage_logs.rs`: `assert_eq!(err.failure_reason(), ("http_status",
    Some(502)))` → assert `.reason` and `.status`; same for the `decode` assertion.
  Run `grep -rn "failure_reason" crates` afterwards: every hit must be a `Failure`, except
  `join_failure_reason` in `crates/flags-server/src/main.rs` (a `JoinError` helper, out of scope).

- [x] **Step 5: Run the tests to verify they pass**
  Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --lib`
  then all gates in Global Constraints (test, lint, format, extra, podman build). Expected: all pass, including
  `crates/flags-server/tests/log_text.rs`.

- [x] **Step 6: Commit**
  `feat(core): log the sqlstate and transport sub-kind with failure reasons` — body: why (an
  operator at `info` can't tell a Postgres restart from a data exception, or a refused connection
  from a timeout), what (one `Failure` type, named 57/25 classes), ends `Fixes #43`. No attribution.

---

### Task 2: Prove the new fields reach the MCP and SDK log lines

**Files:**
- Modify: `crates/flags-mcp/tests/failure_logs.rs`
- Modify: `crates/flags-api/tests/failure_logs.rs`

**Interfaces:** consumes the log field names from Task 1.

**Out-of-band / invariants:** none. Keep each file a single-test binary (callsite interest
caching; see its module doc).

**Model:** sonnet — test additions following an existing pattern.

- [x] **Step 1: Write the tests**
  - Both files: add a private `Sqlstate(&'static str)` test type implementing
    `sqlx::error::DatabaseError`, modelled on `FakeDbError` in `crates/flags-core/src/error.rs`
    tests but with two fields, a code and a marker: `code()` returns the code, and both
    `Display` and `message()` write the marker (sqlx formats `Error::Database` through the boxed
    error's `Display`, so that is what reaches the debug line). Add a case
    `Error::Db(sqlx::Error::Database(Box::new(FakeDbError("57P01", "MARKER_SQLSTATE"))))` with
    expected fields `code="internal_error" reason="admin_shutdown" sqlstate="57P01"` above debug and
    the marker only at debug. In flags-api, through `ApiError::from`; in flags-mcp, through
    `from_core`.
  - `crates/flags-mcp/tests/failure_logs.rs`: a `from_platform` transport case. Produce the
    error before and outside the `capture` closure (so reqwest/hyper events from the connect
    attempt are not captured; only `from_platform` runs inside it), with a current-thread tokio runtime (`tokio::runtime::Builder::new_current_thread()
    .enable_all().build()`) calling `otto_resource::PlatformClient::usage_status` against a closed
    local port (as in Task 1); expected fields
    `code="platform_unavailable" reason="transport" transport="connect"`. The marker for this
    case is the error's own text: assert the ERROR line does not contain `"could not reach the
    platform"` and a DEBUG line does.
  - If either crate's `[dev-dependencies]` lacks `tokio` with `rt`/`net`, or `uuid`, add it from
    the workspace (`tokio = { workspace = true, features = ["full"] }`); never add `reqwest`.

- [x] **Step 2: Verify they fail without the fields**
  Temporarily delete `sqlstate = f.sqlstate(),` from `log_server_failure`'s error event; run
  `cargo test -p flags-mcp --test failure_logs`; expected FAIL naming `sqlstate="57P01"`. Restore
  it. Do the same for the `flags-api` SDK site with `cargo test -p flags-api --test failure_logs`.

- [x] **Step 3: Run them to verify they pass**, then all gates in Global Constraints (test, lint,
  format, extra, podman build).

- [x] **Step 4: Commit**
  `test: prove sqlstate and transport fields reach mcp and sdk logs` — body: why, ends
  `Fixes #43`. No attribution.
