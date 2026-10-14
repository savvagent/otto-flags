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
  logs `tracing::error!(error = %e, …)` when the revocation-tombstone check fails (a database
  error, `auth.rs:244`) and when token introspection fails (a platform error, `auth.rs:270`).
  The title says "MCP error logging", so these two are in scope.
- It also exists outside `flags-mcp`, at `warn`/`error`: `flags-api/src/error.rs:52`,
  `flags-api/src/platform.rs:44,51`, `flags-core/src/notify.rs` (4), `flags-core/src/usage.rs`
  (3), `flags-server/src/main.rs` (4), `flags-server/src/health.rs:65`. Those are out of scope
  here and go to #35 (filed by this run), so this PR stays the size #29 describes.
- `Error::code()` maps every platform failure to `platform_unavailable`, the wire code an agent
  branches on. That is right for the wire but useless to an operator: it cannot tell "the platform
  rejected our credential" (a deployment fault that needs a person now) from a transient 503. The
  log needs a finer, still text-free code.

## 3. Assumptions

- **Logged code for platform failures.** A new `pub(crate) fn platform_code(&otto_resource::Error)
  -> &'static str` in `flags-mcp/src/error.rs` maps each variant to a fixed string:
  `platform_unauthorized`, `platform_not_found`, `platform_http_status`, `platform_transport`,
  `platform_decode`, `platform_invalid_url`. The variant is chosen by us, never by the platform, so
  it is safe above debug. The HTTP status number is not logged above debug (the issue asks for the
  code only; the status is in the debug text). Rationale: the smallest change that still separates
  a credential fault from an outage.
- **One logging helper.** `pub(crate) fn log_server_failure(what: &'static str, code: &'static str,
  detail: &dyn Display)` emits `tracing::error!(code, "{what}")` then
  `tracing::debug!(code, error = %detail, "{what}")`. All four sites call it, so one unit test
  covers the rule and a future site has one obvious thing to call. The wording of each `what`
  message stays as today.
- **`from_core` on `Error::Platform(p)`** logs `platform_code(p)`; every other internal error logs
  `e.code()` (`internal_error`, or the tenant error's code).
- **Wire unchanged.** `ErrorData` message, `data.code`, and `data.retriable` are byte-for-byte what
  they are today; `platform_code` is used only in logs.
- **Test-only dependency.** `tracing-subscriber` (already a workspace dependency, already in
  `Cargo.lock`) becomes a `flags-mcp` dev-dependency to capture log output in a unit test. No
  production dependency changes.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

No database or platform error text is logged above `debug` by `flags-mcp`; operators still see at
`error` which kind of failure happened.

- `from_core` on an internal error, `from_platform`, and both `auth.rs` failure paths log one
  `error` event carrying `code` and no error text, plus one `debug` event carrying the text.
- A unit test captures output at `DEBUG` for a database error whose text contains a marker and a
  platform `Status` error whose body contains a marker: the `ERROR` lines contain the code and not
  the marker; the `DEBUG` lines contain the marker.
- The existing `error.rs` tests (wire codes, retriable flags, no database text in messages) pass
  unchanged.
- `grep -rnE 'tracing::(error|warn|info)!\([^)]*error = %' crates/flags-mcp/src` finds nothing
  (a supplement to the test: it misses calls rustfmt wraps across lines).

## 5. Scope

**In:** `crates/flags-mcp/src/error.rs`, `crates/flags-mcp/src/auth.rs`,
`crates/flags-mcp/Cargo.toml` (dev-dependency), `Cargo.lock` if cargo rewrites it.

**Out:** logging in `flags-api`, `flags-core`, `flags-server` (#35); any change to the
wire shape of errors; log format or subscriber configuration in `flags-server`.

## 6. Architecture

- `error.rs`: add `platform_code` and `log_server_failure`. `from_core`'s internal branch
  (`error.rs:19`) calls `log_server_failure("server-side failure surfaced to an MCP caller", code,
  e)`. `from_platform` (`error.rs:57`) calls it with `platform_code(e)`. The module doc gains one
  bullet: server-side failures are logged by code above debug, text at debug.
- `auth.rs:244` → `log_server_failure("could not check the revocation tombstones", e.code(), &e)`
  (the error is a `flags_core::Error`). `auth.rs:270` → `log_server_failure("token introspection
  failed", platform_code(&e), &e)` (an `otto_resource::Error`).
- No public interface changes: both helpers are `pub(crate)`.

## 7. Invariants impact

- **SDK-reported text is untrusted; database error text never reaches a tool response:** responses
  are unchanged; this extends the same care to logs.
- **MCP auth:** behaviour of `require_bearer` unchanged (same 401/503 responses); only its logging.
- No migration, no SQL, no tenant query, no metering, no new config or scope. Others unaffected.

## 8. Migration

None.

## 9. Out-of-band

None. Deploy is CI-auto on merge; verification reads `fly logs` for the boot markers and calls
`whoami` over MCP. Debug-level text is invisible in production at the default `info` filter, which
is the point.

## 10. Error handling and edge cases

- `Decode(String)` and `Status { body }` carry platform text: debug only.
- `Transport(reqwest::Error)` text can include the platform URL: debug only.
- A `tracing` filter at `debug` (local development) still shows everything, so local debugging
  loses nothing.

## 11. Testing approach

- Unit test in `crates/flags-mcp/src/error.rs` `mod tests`: a `tracing_subscriber::fmt` subscriber
  with ANSI off, level `DEBUG`, writing to a shared in-memory buffer, installed with
  `tracing::subscriber::with_default`; drive `from_core(&Error::Db(sqlx::Error::Protocol(<marker>)))`
  and `from_platform(&otto_resource::Error::Status { status: 503, body: <marker> })`; split lines
  by level and assert as in §4.
- A unit test for `platform_code` covering every variant that can be built in a test.
- `auth.rs` call sites are covered by construction (they call the tested helper); existing e2e
  tests in `crates/flags-server/tests/e2e.rs` keep covering their responses.
- Gates: test (with `DATABASE_URL`), clippy, fmt, the migrations guard, and `podman build`
  (`crates/**` and `Cargo.lock` match).

## 12. Risks and open questions

- An operator alert keyed on the old error text ("platform returned HTTP …") stops matching. No
  such alert exists (no observability is wired to Fly logs); noted for completeness.
