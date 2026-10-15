# Rust SDK Server Keys and Telemetry Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The Rust SDK accepts `srv_` keys and reports evaluations and errors per the SDK guide.

**Architecture:** A crate-private `telemetry` module queues events and sends batches from a lazily
spawned background task and on `flush()`; `FlagClient` tracks answered lookups and exposes
`track_error`/`flush`.

**Tech Stack:** Rust 2021, tokio, reqwest 0.11, serde_json, wiremock.

**Ticket:** #93

**Spec:** `docs/specs/2026-10-15-rust-sdk-server-keys-telemetry-design.md`

## Global Constraints

- Gates, from `packages/rust-server` and from `examples/rust-server`, with
  `export CARGO_BUILD_JOBS=3 TMPDIR=<worktree>/packages/rust-server/target/tmp` (mkdir first):
  `cargo fmt` then `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings`;
  `cargo test --locked`.
- Evidence: pass counts per test binary (SDK: lib, dynamic_config_test, integration_test,
  override_test, telemetry_test, doc-tests; example: 1 binary, 6+ tests).
- Invariants: never throw on a lookup; documented REST bodies only; no end-user context sent;
  key never logged; bounded queues and batched requests; MIT; public API additive.
- Worktree only; trunk only via the PR. Commit format `feat(sdk): <imperative, lower-case>`,
  why-body, `Fixes #93`; no attribution of any kind. Commit from the worktree root.

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `packages/rust-server/src/telemetry.rs` | Queue, bound, truncation, batching sender, background flusher |
| **Create.** `packages/rust-server/tests/telemetry_test.rs` | Wiremock tests for telemetry |
| **Modify.** `packages/rust-server/src/client.rs` | Key prefixes, tracking calls, `track_error`, `flush` |
| **Modify.** `packages/rust-server/src/types.rs` | `ErrorReport`, `Config::with_telemetry` |
| **Modify.** `packages/rust-server/src/lib.rs` | `mod telemetry`, re-export `ErrorReport`, docs |
| **Modify.** `packages/rust-server/tests/integration_test.rs` | `srv_` accepted; error message |
| **Modify.** `packages/rust-server/README.md` | Keys, telemetry section |
| **Modify.** `examples/rust-server/src/main.rs`, `examples/rust-server/README.md` | Accept/document `srv_`; drop stale limitations |

## Task Order & Rationale

Task 1 (keys) is independent and small; Task 2 (telemetry) builds on the client; Task 3 (example)
documents both.

---

### Task 1: Accept `srv_` keys

**Model:** haiku
- [ ] Failing test in `tests/integration_test.rs`: `FlagClient::new(Config::new("srv_x"))` is
  `Ok`; `InvalidApiKey.to_string()` contains `sdk_` and `srv_`. Run, see fail.
- [ ] `client.rs`: `if !(key.starts_with("sdk_") || key.starts_with("srv_"))`; error text
  `"Invalid API key: must start with 'sdk_' (client key) or 'srv_' (server key)"`. Run, pass.
- [ ] Commit `feat(sdk): accept server keys in the rust sdk`.

### Task 2: Telemetry

**Model:** sonnet
- [ ] Failing tests in `tests/telemetry_test.rs` (each mounts `POST /api/telemetry/evaluations`
  and/or `/errors` with `header("Authorization", "Bearer srv_test")` and inspects
  `server.received_requests()`):
  1. two answered lookups (one cached) + `flush()` → one request, 2 events with `flag_key`,
     `result`, `environment`, `timestamp`, and no `user_id`/`context` keys;
  2. a 500 evaluate and an invalid key + `flush()` → no telemetry request;
  3. `track_error(ErrorReport::new(..).with_stack_trace(..))` + `flush()` → one errors request
     with the seven fields, and a 5 000-char message arrives as 2 000 chars;
  4. 100 lookups with no `flush()` → an evaluations request arrives (poll
     `received_requests` up to ~2 s);
  5. `with_telemetry(false)` + lookups + `flush()` → nothing;
  6. telemetry endpoint 500 → lookups unaffected, `flush()` returns.
  Run, see fail (missing methods → compile error counts).
- [ ] Implement `src/telemetry.rs` per spec §6 (constants `FLUSH_INTERVAL = 5 s`,
  `FLUSH_AT = 100`, `MAX_EVALUATIONS_PER_REQUEST = 1_000`, `MAX_ERRORS_PER_REQUEST = 100`,
  `MAX_QUEUED = 10_000`, text caps 200/2 000/8 000/64), `ErrorReport` in `types.rs`,
  `Config::with_telemetry`, client wiring, `lib.rs` re-export; unit tests for truncation on a
  char boundary and the queue bound. Run all gates.
- [ ] README: keys (`srv_` recommended for servers), telemetry section (what is sent, batching,
  `track_error`, `flush()` before shutdown, `with_telemetry(false)`).
- [ ] Commit `feat(sdk): report rust sdk evaluations and errors as telemetry`.

### Task 3: Example

**Model:** haiku
- [ ] `examples/rust-server`: `Settings` error text and docs say the key may be `srv_...`
  (recommended) or `sdk_...`; a unit test that a `srv_` value is accepted; README env table,
  prerequisites, quick-start; remove the stale "Known limitations" items (#91, #93); call
  `flags.flush().await` on shutdown if the example has a shutdown hook, else document it.
- [ ] Run the example gates; commit `docs(examples): use the server key in the rust example`.
