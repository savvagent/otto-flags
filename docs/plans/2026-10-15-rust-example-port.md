# Rust example port Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `examples/rust-server` builds, formats, and passes clippy against the current
`otto-flags` Rust SDK (`FlagClient`, `Config`, `Context`) and axum 0.7 (`axum::serve`), keeps its
routes and response shapes, and its README matches the code.

**Architecture:** Rewrite `src/main.rs` on the SDK's `FlagClient::is_enabled`, which never fails
(it falls back to the configured default), so the handlers lose their error arms. Settings come
from the environment through a small `Settings::from_env` that is unit-tested: the SDK key is
required (its format is left to `FlagClient::new`, which refuses a key without the `sdk_`
prefix), an empty variable counts as unset, the API URL defaults to production
`https://otto-flags.savvagent.com`, the environment to `development`, the port to `8083`. Serve
with `tokio::net::TcpListener` + `axum::serve`.

**Tech Stack:** Rust (stable, edition 2021), axum 0.7.9 (locked), tokio, the `otto-flags` path
dependency.

**Ticket:** #16

**Spec:** fast-path — no spec

## Global Constraints

- Gates, run in `examples/rust-server` with `CARGO_BUILD_JOBS=3` and `TMPDIR=$PWD/target/tmp`
  (create it first):
  - `cargo build --locked` (must exit 0)
  - `cargo fmt --check` (must exit 0)
  - `cargo clippy --locked --all-targets -- -D warnings` (must exit 0)
  - `cargo test --locked` (the new `Settings` unit tests pass; none ignored)
- Evidence: pass/fail/ignored counts per test binary; skipped tests are not a pass.
- Invariants: examples are MIT, no server code copied in; SDKs (and so the example) never throw
  into the host app on a flag lookup; the example uses the SDK client, not a hand-rolled request.
- Do not edit `packages/rust-server` (another change, #4, is in flight there). No dependency or
  version changes; `Cargo.lock` must not change (`--locked`).
- Worktree only; trunk changes only through the PR.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, why-body ending `Fixes #16`; no
  attribution of any kind (no Co-Authored-By, no "Generated with", no credit markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `examples/rust-server/src/main.rs` | Port to `FlagClient`/`Config`/`Context` and `axum::serve`; `Settings::from_env` + unit tests |
| **Modify.** `examples/rust-server/README.md` | Environment variables, run steps, and usage snippet matching the code |
| **Modify.** `CLAUDE.md` | Drop the examples/rust-server known-red line (#16) |

## Task Order & Rationale

One task: the example is a single binary and its README describes it.

---

### Task 1: Port the example

**Files:**
- Modify: `examples/rust-server/src/main.rs`, `examples/rust-server/README.md`, `CLAUDE.md`

**Out-of-band / invariants:** none out-of-band (example-only; nothing deploys it). The server
redeploys unchanged on merge.

**Model:** sonnet

- [ ] **Step 1: Write the failing tests.** Add `#[cfg(test)] mod tests` in `src/main.rs` for
  `Settings::from_env(lookup)`: a missing `OTTO_FLAGS_SDK_KEY` is an error; an empty variable
  counts as unset; a bad `PORT` is an error; with only the key set, the URL is `https://otto-flags.savvagent.com`,
  environment `development`, port `8083`; every variable set is honoured.
- [ ] **Step 2: Run, see it fail.** `cargo test --locked` fails to compile (old imports,
  `axum::Server`).
- [ ] **Step 3: Implement.** Port `main.rs`: `Config::new(key).with_base_url(url)` plus
  `with_default` for each flag the example reads; `FlagClient::new`; `Context::new()
  .with_user_id(..).with_environment(..)` (and `.with_attribute("endpoint", ..)` on `/api/data`);
  `client.is_enabled(key, Some(ctx)).await` (the three `/api/features` lookups concurrently); `TcpListener::bind` + `axum::serve`. Keep `/health`,
  `GET /api/features?userId=`, `POST /api/data` and their JSON shapes.
- [ ] **Step 4: Run, see it pass.** All four gates above.
- [ ] **Step 5: README + CLAUDE.md.** README env vars, defaults, run steps, and snippet match
  `main.rs`, and a Known limitations section links the SDK bugs the example exposes (#91, #92,
  #93); remove the #16 known-red line and add a `cargo test --locked` gate for the example.
- [ ] **Step 6: Commit.** `fix(examples): port the rust example to the current sdk and axum 0.7`
  with a why-body ending `Fixes #16`.
