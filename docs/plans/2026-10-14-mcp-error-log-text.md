# MCP failure logging Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `flags-mcp` logs database and platform failures by a text-free code at `error`, and their
text only at `debug`; tool and HTTP responses are unchanged.

**Architecture:** Task 1 adds the logging helper in `crates/flags-mcp/src/error.rs` and routes the
two sites there and the two in `crates/flags-mcp/src/auth.rs` through it. Task 2 (from PR review)
moves the text-free reason into `flags-core` (`Error::failure_reason`,
`error::platform_failure_reason`) so database failures are told apart too and platform reasons have
one definition, and moves the log-capture test into its own binary.

**Tech Stack:** Rust (stable, MSRV 1.88), tracing 0.1, tracing-subscriber 0.3 (dev only).

**Ticket:** savvagent/otto-flags#29

**Spec:** `docs/specs/2026-10-14-mcp-error-log-text-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-29-mcp-error-log-text`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), run from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`, `Cargo.lock`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434.
- Invariants that apply: database error text never reaches a tool response (responses must stay
  byte-for-byte as they are); MCP auth behaviour (401/503 answers of `require_bearer`) unchanged;
  no SQL outside flags-core (none added).
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #29`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

### Task 1: Log MCP failures by code above debug

> Shipped as written in cab0922, then reshaped by Task 2: `platform_code` and the
> `(what, code, detail)` helper below were replaced by `flags-core`'s failure reasons.

**Model:** sonnet

**Files:**
- Modify: `crates/flags-mcp/Cargo.toml` (`[dev-dependencies]`: `tracing-subscriber.workspace = true`)
- Modify: `crates/flags-mcp/src/error.rs`
- Modify: `crates/flags-mcp/src/auth.rs` (lines ~244 and ~270)
- `Cargo.lock` (the new dev-dependency rewrites `flags-mcp`'s entry; commit it)

- [x] **Step 1: Write the failing tests** in `crates/flags-mcp/src/error.rs` `mod tests`:
  - A capture helper: a `tracing_subscriber::fmt()` subscriber with `.with_ansi(false)`,
    `.with_max_level(tracing::Level::DEBUG)`, and a `MakeWriter` over an
    `Arc<Mutex<Vec<u8>>>`, installed with `tracing::subscriber::with_default` around a closure (thread-local on purpose:
    the functions under test are synchronous, and a global subscriber would leak between parallel
    tests); returns the captured text.
  - `server_failures_log_the_code_above_debug_and_the_text_only_at_debug`: inside the capture, call
    `from_core(&Error::Db(sqlx::Error::Protocol("MARKER_DB relation secret".into())))` and
    `from_platform(&otto_resource::Error::Status { status: 503, body: "MARKER_PLATFORM".into() })`.
    Assert: lines containing ` ERROR ` include `code="internal_error"` and
    `code="platform_http_status"` (tracing-subscriber 0.3 prints `&str` fields quoted), and no line containing ` ERROR ` contains `MARKER_`; lines
    containing ` DEBUG ` contain `MARKER_DB` and `MARKER_PLATFORM`.
  - `platform_codes_name_the_failure_kind`: `platform_code` returns `platform_unauthorized`,
    `platform_not_found`, `platform_http_status`, `platform_decode`, `platform_invalid_url` for
    `Unauthorized`, `NotFound`, `Status{..}`, `Decode(..)`, `InvalidUrl(..)` (`Transport` needs a
    `reqwest::Error` and is not built in the test).
  - Also assert the wire is unchanged in the first test: `from_platform`'s `data.code` is still
    `platform_unavailable` and `from_core`'s Db message is unchanged (no `MARKER_`).
- [x] **Step 2: Run, see them fail to compile / fail:**
  `cargo test -p flags-mcp --lib error::tests` (no DB needed for these unit tests)
- [x] **Step 3: Implement** in `error.rs`:
  - `pub(crate) fn platform_code(e: &otto_resource::Error) -> &'static str` with an exhaustive
    match (`Transport(_)` → `platform_transport`).
  - `pub(crate) fn log_server_failure(what: &'static str, code: &'static str, detail: &dyn std::fmt::Display)`:
    `tracing::error!(code, "{what}"); tracing::debug!(code, error = %detail, "{what}");`
    with a doc comment giving the reason (database and platform text can carry SQL, schema names,
    response bodies, and URLs; the code is ours).
  - `from_core` internal branch: replace the `tracing::error!` with
    `log_server_failure("server-side failure surfaced to an MCP caller", code, e)` where `code` is
    `platform_code(p)` for `Error::Platform(p)`, else `e.code()`.
  - `from_platform`: replace the `tracing::error!` with
    `log_server_failure("platform lookup failed for an MCP caller", platform_code(e), e)`.
  - Module doc: add to the server-side bullet that their text is logged only at debug; the code is
    logged at error.
  - `auth.rs:244`: `crate::error::log_server_failure("could not check the revocation tombstones", e.code(), &e)`.
    `auth.rs:270`: `crate::error::log_server_failure("token introspection failed", crate::error::platform_code(&e), &e)`.
    Confirm the module path (`crate::error`) and the error types by reading the file.
- [x] **Step 4: Run, see them pass:** `cargo test -p flags-mcp --lib error::tests`, then
  `grep -rnE 'tracing::(error|warn|info)!\([^)]*error = %' crates/flags-mcp/src` prints nothing.
- [x] **Step 5: Run every gate** and report pass counts per test binary:
  - `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all -- --check`
  - `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` (prints nothing)
  - `podman build -t otto-flags:check .`
- [x] **Step 6: Commit** `crates/flags-mcp/Cargo.toml`, `Cargo.lock`, `crates/flags-mcp/src/error.rs`,
  `crates/flags-mcp/src/auth.rs` with exactly this message (no trailers of any kind):

  ```
  fix(mcp): log server-side failures by code, text only at debug

  Database and platform error text can carry SQL, schema names, platform
  response bodies, and URLs, and flags-mcp logged it at error level. Log a
  text-free code at error (a finer platform code, so a rejected credential
  is told apart from an outage) and the text only at debug.

  Fixes #29
  ```

The spec's `**Status:**` line and this plan's boxes are updated in the PR's final commit, before
merge, by the orchestrator, not by this task.

### Task 2: Failure reasons in flags-core (PR review round)

**Model:** opus (done by the orchestrator, with the review findings in hand)

Findings addressed: every database failure logged as `internal_error` (rust-pro,
silent-failure-hunter); platform status dropped above debug (security, silent-failure-hunter); a
second list of platform reasons beside `entitlements::failure_reason` (architect); `from_tenant`
flattening tenant database errors into `Config` text (silent-failure-hunter); the log-capture test
flaking about 1 run in 20–40 through tracing's callsite-interest cache (pr-test-analyzer); no test
of the `Error::Platform` path through `from_core` (architect, pr-test-analyzer); spec and plan stale
after a507905 (comment-analyzer, architect).

**Files:**
- Modify: `crates/flags-core/src/error.rs` (`Error::failure_reason`, `platform_failure_reason`,
  `db_failure_reason`, unit test)
- Modify: `crates/flags-core/src/entitlements.rs` (use the moved function; its test moves too)
- Modify: `crates/flags-mcp/src/error.rs` (`log_server_failure` takes `(reason, status)`; drop
  `platform_code`/`log_code`; `respond` split out; `from_tenant` by value; wire test for it)
- Modify: `crates/flags-mcp/src/auth.rs`, `crates/flags-mcp/src/server.rs`
- Create: `crates/flags-mcp/tests/failure_logs.rs` (the capture test, alone in its binary)
- Modify: the spec, and this plan

- [x] **Step 1:** Write `failure_reasons_name_the_kind_without_the_text` in flags-core and the
  capture test in `tests/failure_logs.rs`; see them fail to compile.
- [x] **Step 2:** Implement as the spec's §3 and §6 describe.
- [x] **Step 3:** `cargo test -p flags-mcp` and `cargo test -p flags-core --lib` pass; run
  `cargo test -p flags-mcp` 60 times with no failure.
- [x] **Step 4:** Run every gate (Task 1, Step 5) and report pass counts per test binary.
- [x] **Step 5:** Commit `fix(core): name database and platform failures for logs without their
  text`, body ending `Fixes #29`; no trailers.

