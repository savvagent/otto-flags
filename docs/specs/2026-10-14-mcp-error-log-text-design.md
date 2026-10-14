# MCP failure logging: the code at error level, the text only at debug

**Status:** DRAFT
**Issue:** #29

## 1. Brief

From #29 (found reviewing #24; predates it):

> `crates/flags-mcp/src/error.rs` logs internal errors with `tracing::error!(error = %e, …)`, and
> `from_platform` does the same, so database and platform error text reaches the logs at error
> level. The repo's rule (and the pattern #24's rollback checker follows) is to log the error code
> above debug and the text only at debug.
>
> **Acceptance criteria**
> - [ ] Internal and platform errors are logged with their code at `error`, and their text only at
>   `debug`.
> - [ ] Tool responses are unchanged (they already carry no internal text).

## 2. Premise corrections

- The same pattern exists in the same crate outside `error.rs`: `crates/flags-mcp/src/auth.rs`
  logs `tracing::error!(error = %e, …)` in `require_bearer` when the revocation-tombstone check
  fails (a `flags_core::Error`) and when token introspection fails (an `otto_resource::Error`).
  The title says "MCP error logging", so both are in scope.
- It also exists in `flags-api`, `flags-core`, and `flags-server`. Those are out of scope here and
  tracked in #35, so this PR stays the size #29 describes.
- "Log the code" is not enough on its own. `Error::code()` maps every platform failure to
  `platform_unavailable` and every database failure to `internal_error`: right for the wire, where
  an agent branches on them, but an operator reading logs at `info` could no longer tell a rejected
  platform credential from a 503, or a pool timeout from a deadlock or a missing grant. The log
  needs a finer reason that still carries no text.
- `flags-core/src/entitlements.rs` already had a text-free reason for platform failures
  (`failure_reason`, logged as `reason` and `status`). That becomes the single definition rather
  than a second list in `flags-mcp`.

## 3. Assumptions

- **Reason, defined once in `flags-core`.** `Error::failure_reason(&self) -> (&'static str,
  Option<u16>)` in `crates/flags-core/src/error.rs`:
  - database failures (`Error::Db`, and `Error::Tenant(otto_tenant::Error::Db)`): the sqlx variant
    (`pool_timed_out`, `pool_closed`, `io`, `tls`, `protocol`, `row_not_found`, `decode`), or for
    an error Postgres raised, the condition name of its SQLSTATE (`serialization_failure`,
    `deadlock_detected`, `lock_not_available`, `query_canceled`, `insufficient_privilege`,
    `undefined_table`, `undefined_column`, and by class `integrity_constraint_violation`,
    `connection_exception`, `insufficient_resources`), else `database_error`; `other` for sqlx
    variants added later.
  - platform failures: `pub fn platform_failure_reason(&otto_resource::Error)`, moved from
    `entitlements.rs` unchanged (`http_status` with the status, `transport`, `decode`,
    `unauthorized`, `not_found`, `invalid_url`). Entitlements calls it from its new home, so its
    log lines are unchanged.
  - anything else: `(self.code(), None)`.
  All of these are strings we chose and a status number; nothing comes from database or platform
  text, so they are safe above debug.
- **One logging helper in `flags-mcp`.** `pub(crate) fn log_server_failure(what, code, (reason,
  status), detail: &dyn Display)` emits `tracing::error!(code, reason, status, "{what}")` then
  `tracing::debug!(code, error = %detail, "{what}")`. `code` is the wire code, so logs and
  responses share a search key. All four sites call it. The `what` messages are as before.
- **`from_tenant` takes the error by value** (its one caller, `server.rs`, already owns it). It
  keeps its wire answer exactly (anything but `Invalid` becomes an opaque, non-retriable internal
  error) but logs the original's reason, so a tenant pool timeout is logged as `pool_timed_out`
  rather than as flattened `Config` text. `from_core` is split into logging plus a private
  `respond`, so `from_tenant` logs once.
- **Wire unchanged.** `ErrorData` message, `data.code`, and `data.retriable` are what they were for
  every input; tests pin the platform, database, and tenant answers.
- **Test-only dependency.** `tracing-subscriber` (already a workspace dependency) becomes a
  `flags-mcp` dev-dependency. No production dependency changes.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

No database or platform error text is logged above `debug` by `flags-mcp`; operators still see at
`error` which kind of failure happened.

- `from_core` on an internal error, `from_tenant` on an internal error, `from_platform`, and both
  `require_bearer` failure paths log one `error` event carrying `code`, `reason` (and `status` for
  an HTTP failure) and no error text, plus one `debug` event carrying the text.
- A test captures output at `DEBUG` for database, platform-in-core, raw platform, and tenant
  failures: the `ERROR` lines carry the expected `code`/`reason`/`status` and not the marker; the
  `DEBUG` lines carry the marker.
- `failure_reason` and `platform_failure_reason` are unit-tested for each reason they can be built
  to produce.
- Existing `error.rs` tests (wire codes, retriable flags, no database text in messages) pass, and a
  new one pins `from_tenant`'s wire answers.
- `grep -rnE 'tracing::(error|warn|info)!\([^)]*error = %' crates/flags-mcp/src` finds nothing
  (a supplement to the tests: it misses calls rustfmt wraps across lines).

## 5. Scope

**In:** `crates/flags-mcp/src/{error,auth,server}.rs`, `crates/flags-mcp/tests/failure_logs.rs`,
`crates/flags-mcp/Cargo.toml` (dev-dependency), `Cargo.lock`, `crates/flags-core/src/error.rs`,
`crates/flags-core/src/entitlements.rs` (the moved function).

**Out:** logging in `flags-api`, `flags-core`, `flags-server` other than entitlements' moved helper
(#35, which can reuse `failure_reason`); any change to the wire shape of errors; log format or
subscriber configuration in `flags-server`.

## 6. Architecture

- `flags-core` owns what a failure is called (`code()` for the wire, `failure_reason()` for logs),
  next to `is_transient`, which already reads the same SQLSTATEs. `flags-mcp` owns how it logs.
  The dependency direction (`flags-mcp` → `flags-core`) is unchanged.
- `flags-mcp/src/error.rs`: `log_server_failure`; `from_core` logs internal errors then calls
  `respond` (the old body); `from_tenant` builds the wire error as before, logs the original's
  reason, and calls `respond`; `from_platform` logs `platform_unavailable` with
  `platform_failure_reason`. The module doc states the log rule.
- `require_bearer` in `auth.rs`: the tombstone failure logs `e.code()` and `e.failure_reason()`;
  the introspection failure logs `platform_unavailable` and `platform_failure_reason(&e)`.
- Public interface: `flags_core::Error::failure_reason` and
  `flags_core::error::platform_failure_reason` are additive. `flags_mcp::error::from_tenant` now
  takes `otto_tenant::Error` by value (internal to the workspace; one caller).

## 7. Invariants impact

- **Database error text never reaches a tool response:** responses are unchanged; this extends the
  same care to logs.
- **MCP auth:** `require_bearer`'s 401/503 answers are unchanged; only its logging.
- No migration, no SQL, no tenant query, no metering, no new config or scope. Others unaffected.

## 8. Migration

None.

## 9. Out-of-band

None. Deploy is CI-auto on merge; verification reads `fly logs` for the boot markers and calls
`whoami` over MCP. Production runs at `info` (`fly.toml`), so the debug text is not logged there.
Turning `flags_mcp=debug` on in production brings the text back; that is the intended escape hatch
for diagnosis and should be brief.

## 10. Error handling and edge cases

- `Decode(String)`, `Status { body }`, and `Transport` (whose text can include the platform URL)
  carry platform text: debug only.
- A Postgres error's message can quote values and schema; only its SQLSTATE condition name is
  logged above debug.
- `sqlx::Error` is `#[non_exhaustive]`: a variant added later logs `other` rather than failing to
  compile. `otto_resource::Error` is exhaustive at the pinned rev, so a new platform variant fails
  the build until it is named.
- At `debug` (local development) both events appear, so local debugging loses nothing.

## 11. Testing approach

- `crates/flags-mcp/tests/failure_logs.rs`: one test in its own binary. `tracing` caches per
  callsite whether any subscriber is interested, and other unit tests reach the same callsites with
  none installed, which made an in-crate version of this test drop its debug events about 1 run in
  20–40. A `tracing_subscriber::fmt` subscriber (ANSI off, `DEBUG`, in-memory writer) installed
  with `with_default` around each case; assertions on presence or absence, never line counts.
- `crates/flags-core/src/error.rs` unit test: `failure_reason` and `platform_failure_reason` over
  database (variants and SQLSTATEs, through `Tenant` too), platform, and non-internal errors. The
  test that lived in `entitlements.rs` for the moved function folds into it.
- `crates/flags-mcp/src/error.rs` unit test: `from_tenant`'s wire answers for `Invalid`,
  `OrgNotFound`, and `Db`.
- The `require_bearer` call sites are covered by construction (they call the tested helper with
  the tested reason functions); which arguments each passes is checked by review, not by a test.
  Existing e2e tests keep covering their responses.
- Gates: test (with `DATABASE_URL`), clippy, fmt, the migrations guard, and `podman build`.

## 12. Risks and open questions

- An operator search keyed on the old error text ("platform returned HTTP …") stops matching at
  `info`; search by `code`/`reason` instead. No alert is wired to Fly logs today.
- `Transport` is not built in a test (it needs a `reqwest::Error`); its arm is a fixed string and
  unchanged from the entitlements version.
