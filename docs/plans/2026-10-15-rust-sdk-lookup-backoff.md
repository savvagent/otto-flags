# Rust SDK Lookup Backoff and Response Cap Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** After a failed lookup the Rust SDK skips requests for a short cooldown (honouring
`Retry-After`), logs once per window, and reads at most 1 MiB of an evaluate response.

**Architecture:** A crate-private `Backoff` (`src/backoff.rs`) holds a client-wide window and a
bounded per-flag map behind an `Arc<Mutex<_>>`; `FlagClient::evaluate` consults it before `fetch`
and records each `fetch` failure with its scope and any `Retry-After`. `fetch` reads the body
through a capped chunk reader.

**Tech Stack:** Rust 2021, reqwest 0.11, serde_json, wiremock 0.6, tokio.

**Ticket:** #111

**Spec:** `docs/specs/2026-10-15-rust-sdk-lookup-backoff-design.md`

## Global Constraints

- Gates, run from `packages/rust-server` and, for fmt and clippy, from `examples/rust-server`, with
  `export CARGO_BUILD_JOBS=3 TMPDIR=<worktree>/packages/rust-server/target/tmp` (mkdir it first):
  `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings`; `cargo test --locked`
  (SDK only). Run `cargo fmt` before `--check`.
- Evidence: pass counts per test binary (lib unit tests, backoff_test, dynamic_config_test,
  integration_test, override_test, telemetry_test, doc-tests); a test that did not run is not a
  pass.
- Invariants: a lookup never returns `Err` or panics into the host; the SDK REST contract and
  request shape are unchanged; MIT package, no server code copied; public API changes are only
  `Config::failure_cooldown` + `Config::with_failure_cooldown` (spec §3).
- No version bump; the crate is unpublished. No changeset (not a pnpm package).
- Worktree only; trunk changes only through the PR.
- Commit format: `feat(sdk): <imperative, lower-case>`, a why-body ending `Fixes #111`; no
  attribution of any kind (no Co-Authored-By, no "Generated with", no credit markers). Commit from
  the worktree root.

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `packages/rust-server/src/backoff.rs` | Cooldown windows (client, per flag), `Retry-After` parsing, unit tests |
| **Modify.** `packages/rust-server/src/lib.rs` | `mod backoff;` |
| **Modify.** `packages/rust-server/src/types.rs` | `Config::failure_cooldown`, default 5 s, `with_failure_cooldown` |
| **Modify.** `packages/rust-server/src/client.rs` | Skip during a cooldown, record failures with scope, capped body read, docs |
| **Create.** `packages/rust-server/tests/backoff_test.rs` | Wiremock tests for the AC |
| **Modify.** `packages/rust-server/tests/integration_test.rs` | Tests expecting every failed lookup to ask use a zero cooldown |
| **Modify.** `packages/rust-server/README.md` | Config builder line, Error Handling paragraph |

## Task Order & Rationale

Task 1 builds the pure window logic with unit tests (no I/O). Task 2 wires it and the body cap
into the client, test-first with wiremock, and updates docs. Task 3 records the docs as shipped.

---

### Task 1: `Backoff` window logic

Model: sonnet

**Files:** create `packages/rust-server/src/backoff.rs`; modify `packages/rust-server/src/lib.rs`
(`mod backoff;`).

Behavior (spec §3, §6):
- `pub(crate) enum Scope { Client, Flag }`.
- `pub(crate) struct Backoff { cooldown: Duration, state: Arc<Mutex<State>> }`, `#[derive(Clone)]`;
  `State { client_until: Option<Instant>, flags: HashMap<String, Instant> }`.
- `pub(crate) fn new(cooldown: Duration) -> Self`.
- `pub(crate) fn remaining(&self, flag: &str, now: Instant) -> Option<Duration>`: `None` if
  `cooldown` is zero; else the remaining time of `client_until` if in the future, else of the
  flag's entry if in the future; else `None`. A poisoned lock → `None`.
- `pub(crate) fn record(&self, flag: &str, scope: Scope, retry_after: Option<Duration>, now:
  Instant) -> Duration`: returns `Duration::ZERO` and records nothing if `cooldown` is zero.
  Window = `max(cooldown, min(retry_after.unwrap_or(ZERO), MAX_RETRY_AFTER))` with
  `MAX_RETRY_AFTER = 60 s`. `until = now.checked_add(window)`; `None` → record nothing, return
  window. `Scope::Client`: `client_until = max(existing, until)`. `Scope::Flag`: if the map holds
  `MAX_FLAG_ENTRIES` (256) entries and not this flag, first drop entries `<= now`; if still full,
  record nothing; else insert/overwrite. A poisoned lock records nothing.
- `pub(crate) fn parse_retry_after(value: &str) -> Option<Duration>`: trimmed ASCII digits only
  (`u64` parse) → seconds; anything else (HTTP-date, negative, empty) → `None`.
- Module doc comment explaining why per-flag vs client scope (a 404 for one flag must not black
  out others, #111) and why the map is bounded.

Steps:
- [ ] Write unit tests in `#[cfg(test)] mod tests` of `backoff.rs` using a fixed `let t0 =
  Instant::now();` and `t0 + Duration::from_millis(..)`:
  client window blocks every flag until it ends (exactly at end → `None`); flag window blocks only
  that flag; `retry_after` longer than cooldown extends the window, capped at 60 s, shorter than
  cooldown gives the cooldown; zero cooldown: `record` returns zero and `remaining` is always
  `None` (even with `retry_after`); a later client failure with a shorter window does not shorten
  `client_until`; per-flag map: 256 live entries then a 257th is not recorded, but after they
  expire a new one is (pruned); `parse_retry_after("5")`, `" 7 "`, `"0"`, `"-1"`, `""`,
  `"Wed, 21 Oct 2015 07:28:00 GMT"`.
- [ ] Add `mod backoff;` to `lib.rs` and the type stubs so the tests compile; run `cargo test
  --locked --lib` from `packages/rust-server` and see the new tests fail.
- [ ] Implement; run `cargo test --locked --lib` and see them pass; run `cargo clippy --locked
  --all-targets -- -D warnings` (the module is unused by the client until Task 2: add no
  `allow(dead_code)`; instead do Task 1 and Task 2 in one commit if clippy flags dead code).
- [ ] Do not commit separately if dead-code warnings would fail clippy; otherwise commit
  `feat(sdk): add a cooldown window for failed rust sdk lookups` with a why-body, `Refs #111`.

### Task 2: Skip lookups during a cooldown and cap the body

Model: sonnet

**Files:** modify `packages/rust-server/src/types.rs`, `packages/rust-server/src/client.rs`,
`packages/rust-server/tests/integration_test.rs`, `packages/rust-server/README.md`; create
`packages/rust-server/tests/backoff_test.rs`.

Behavior:
- `Config.failure_cooldown: Duration` (doc: how long lookups return their default without a
  request after a failed one; `Retry-After` on 429/503 can lengthen it to 60 s; `Duration::ZERO`
  turns it off), default `Duration::from_secs(5)`; `Config::with_failure_cooldown(Duration)`.
- `FlagClient` gains `backoff: Backoff` (`Backoff::new(config.failure_cooldown)`).
- `evaluate`: after the cache hit branch, before `fetch`:
  `if let Some(left) = self.backoff.remaining(flag_key, Instant::now())` → `tracing::debug!`
  ("Flag {:?} lookup skipped: backing off for {:?} after a failed lookup; using its default")
  and `return self.get_default_result(flag_key);`.
- On `fetch` error: `let window = self.backoff.record(flag_key, failure.scope,
  failure.retry_after, Instant::now());` then the existing `error`/`warn` line, with
  `; next lookup in {window:?}` appended when `window` is non-zero.
- `Failure` gains `scope: Scope` (default `Client` in `Failure::new`) and `retry_after:
  Option<Duration>`. In `fetch`: non-2xx → `scope = Flag` for 404, `retry_after` =
  `parse_retry_after` of the `Retry-After` header for 429 and 503. The `evaluate_url` error keeps
  `Scope::Client`.
- `const MAX_RESPONSE_BYTES: usize = 1024 * 1024;` and `async fn read_capped(response:
  reqwest::Response, cap: usize) -> Result<Vec<u8>, Failure>`: if `content_length() > cap` →
  `Failure` "the body is larger than 1 MiB" with `scope: Flag`; else loop `response.chunk()`,
  refusing the same way once the total would exceed `cap`; a chunk error → `Failure::new("reading
  the body failed: {}", e.without_url())`. Then `serde_json::from_slice` (error → "the body is not
  JSON").
- Update the `evaluate` doc comment: mention the cooldown and the cap.
- README: add `.with_failure_cooldown(Duration::from_secs(5)) // the default` to the Config
  builder example, and a paragraph in Error Handling describing the cooldown, its scopes,
  `Retry-After`, one warning per window, and the 1 MiB cap.

Steps:
- [ ] Write `tests/backoff_test.rs` (wiremock, same helpers style as `integration_test.rs`, key
  `sdk_test_key`), each with `with_default(.., true)` so fallbacks are distinguishable:
  1. `requests_during_the_cooldown_are_skipped_and_the_next_one_after_reaches_the_server`:
     cooldown 300 ms; a 500 mock; count with `server.received_requests()`:
     first lookup (500) → 1 request; three more lookups for two flags immediately → still 1 request,
     each `reason == "error"`; sleep 350 ms; next lookup → 2 requests.
  2. `retry_after_on_503_holds_the_window_past_the_cooldown`: cooldown 100 ms, 503 with
     `Retry-After: 1`; after 1 request, sleep 300 ms, lookup → still 1 request; sleep to 1.1 s
     total, lookup → 2 requests. Same for 429 (parameterize via a helper).
  3. `a_missing_flag_does_not_stop_lookups_of_other_flags`: 404 for `missing`, 200 for `present`;
     after `missing` fails, `present` evaluates (`reason == "evaluated"`) and `missing` again is
     skipped (request count for its path stays 1).
  4. `an_oversized_body_falls_back_for_that_flag_only`: 200 with a JSON body
     `{"enabled":false,"pad":"<MAX+1 bytes of x>"}` for `big` (wiremock sets `Content-Length`) →
     `reason == "error"`, value = default `true`; then `other` (200 `{"enabled":true}`) evaluates
     (per-flag scope). The chunk-loop path (no `Content-Length`) cannot be produced with wiremock;
     cover it with a unit test of the cap arithmetic if extracted, else note it in the report.
  5. `a_zero_cooldown_asks_the_server_every_time`: zero cooldown, 500, two lookups → 2 requests.
- [ ] Run `cargo test --locked --test backoff_test`, see it fail (compile error on
  `with_failure_cooldown` counts as failing first).
- [ ] Implement `types.rs`, `client.rs` changes.
- [ ] Run the whole suite. Every existing test (in `tests/integration_test.rs`,
  `tests/dynamic_config_test.rs`, `tests/override_test.rs`, `tests/telemetry_test.rs`) that fails
  because a later lookup on the same client is now skipped gets `.with_failure_cooldown(
  Duration::ZERO)` and a comment saying why (known: `unknown_flag_falls_back_to_the_default`,
  `falls_back_uncached_on`). Change nothing else in them; list each in the report.
- [ ] Run all gates (SDK fmt/clippy/test; example fmt/clippy). Record per-binary counts.
- [ ] Update README and doc comments.
- [ ] Commit `feat(sdk): back off after failed rust sdk lookups and cap the response read`, why-body,
  `Fixes #111`.

### Task 3: Record as shipped

Model: haiku

- [ ] After the PR exists: spec `**Status:**` → `IMPLEMENTED (#<PR>)`; tick this plan's boxes;
  commit `docs(sdk): mark rust sdk lookup backoff implemented`, `Refs #111`.
