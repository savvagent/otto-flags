# Rust SDK accepts server keys and reports telemetry

**Status:** DRAFT
**Issue:** #93

## 1. Brief

The Rust server SDK accepts only the public client key (`sdk_`) and sends no telemetry, but
`docs/SDK-DEVELOPER-GUIDE.md` says server-side SDKs use the secret server key (`srv_`), and only
telemetry reported with `srv_` drives auto-rollback by default. `FlagClient::new` returns
`InvalidApiKey` for any key not starting with `sdk_`. `Config::enable_telemetry` exists but
nothing posts to `/api/telemetry/evaluations` or `/api/telemetry/errors`. Effect: a Rust service
cannot use its server key, and auto-rollback policies get no evaluations or errors from Rust apps.

Acceptance criteria (#93):
- `FlagClient::new` accepts `srv_` keys as well as `sdk_` keys; the error message names both.
- Decide (and document in the SDK README) whether this SDK reports telemetry per the guide; if
  yes, implement it, if no, remove `enable_telemetry` or mark it unimplemented.
- `examples/rust-server` accepts and documents `srv_` once the SDK does.

## 2. Premise corrections

- The server's telemetry endpoints (`crates/flags-api/src/telemetry.rs`) read, per evaluation,
  only `flag_key`, `result`, `variation`, `environment` (or `context.environment`) and
  `timestamp`; `user_id` and the rest of `context` are ignored. Per error they read `flag_key`,
  `flag_enabled`, `error_type`, `error_message`, `stack_trace`, `environment`, `timestamp`, and
  truncate type/message/stack to 200/2 000/8 000 characters. Batches are capped at 1 000 events
  and axum's default JSON body limit is 2 MB.
- `examples/rust-server/README.md` "Known limitations" still lists #91 (fixed by #103) beside #93;
  both go.

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- **Decision: implement telemetry**, on by default (`enable_telemetry` already defaults to `true`),
  because auto-rollback needs both evaluations and errors from server SDKs.
- **What is sent.** Evaluations: `flag_key`, `result`, `variation`, `environment` (the evaluation
  context's), `timestamp` (Unix seconds). No `user_id` or `context`: the server ignores them, and
  not sending end-user data is the safer default. Errors: the seven fields the server reads, each
  text field truncated client-side to the server's limit (on a char boundary) so a batch stays
  well under the body limit.
- **Which lookups count.** Only results that carry the server's answer (`reason` `"evaluated"` or
  `"cached"`), with the server's (or cached) value and variation, not a local variation override,
  which would skew flag health. A fallback (`"error"`) is not
  an evaluation of the flag, and a refused key names no flag. One event per lookup, cached or not:
  the server counts lookups.
- **Errors are explicit.** New `FlagClient::track_error(ErrorReport)`; `ErrorReport::new(flag_key,
  flag_enabled, error_type, error_message)` with `with_stack_trace` / `with_environment`, and
  `ErrorReport::from_error(flag_key, flag_enabled, &error)` (type from `std::any::type_name`,
  message from `Display`). The SDK cannot see host errors on its own.
- **Delivery.** Events queue in memory and are sent in batches: every 5 s by a background task,
  as soon as either queue reaches 100 events, and on `FlagClient::flush().await`. A request
  carries at most 1 000 events and about 1 MiB of serialized JSON (measured per event, so
  multi-byte or escape-heavy text cannot push a batch past the server's 2 MB body limit; a
  character cap alone would not). The queue is bounded at
  10 000 events per kind; past that, new events are dropped and counted, and the next flush logs
  how many. A failed send (transport error or non-2xx) is logged at `warn` and the batch is
  dropped: telemetry never retries, never blocks or fails a lookup, and never returns `Err`.
- **Background task.** Started on the first queued event when a Tokio runtime is current
  (`Handle::try_current`), so `FlagClient::new` stays synchronous and never panics outside a
  runtime. It waits on a `sleep` (not an `interval`, whose first tick is immediate) or an
  `Arc<Notify>` it owns, and upgrades its `Weak` reference only to send, so it never keeps the
  client alive; dropping the last client wakes it and it exits. A drop guard, built before the
  task is spawned, clears the "running" flag however the task ends (including when its runtime
  shuts down, even before its first poll), so an event in a later runtime starts a new task.
  A host should call `flush().await` before shutdown; dropped clients do not flush (no async drop).
- `Config::with_telemetry(bool)` is added; `enable_telemetry: false` queues nothing.
- Telemetry is sent with whatever key the client holds (the guide: a server SDK uses `srv_`).
- No version bump or changeset (unpublished crate).

## 4. Goal and success criteria

- `FlagClient::new` accepts `srv_...`; any other prefix is `InvalidApiKey`, whose message names
  `sdk_` and `srv_`.
- After lookups and `flush()`, the mock receives `POST /api/telemetry/evaluations` with the
  client's bearer key and one event per answered lookup (`flag_key`, `result`, `variation`,
  `environment`, `timestamp`, no `user_id`/`context`); fallbacks send nothing.
- `track_error` + `flush()` posts `/api/telemetry/errors` with the report, text truncated.
- 100 queued events trigger a send without `flush()`.
- `with_telemetry(false)` sends nothing.
- A telemetry endpoint answering 500, or unreachable, changes no lookup result and `flush()`
  completes.
- `examples/rust-server` accepts a `srv_` key and documents it as the recommended key.

## 5. Scope

In: `src/telemetry.rs` (new, crate-private queue and sender), `src/client.rs` (key check,
tracking, `track_error`, `flush`), `src/types.rs` (`ErrorReport`, `Config::with_telemetry`),
`src/lib.rs` (re-export `ErrorReport`; module docs), tests (new `tests/telemetry_test.rs`),
`README.md`; `examples/rust-server` (`README.md`, `src/main.rs` messages and docs; the env var
keeps its name `OTTO_FLAGS_SDK_KEY` so existing setups keep working).

Out: retries and on-disk buffering; SSE (`enable_realtime`); wrapping host callbacks (TS
`withFlag`-style); server changes.

## 6. Architecture

`src/telemetry.rs`:

- `Telemetry { inner: Arc<Inner> }` (Clone), `Inner { http, base_url, api_key, enabled,
  queues: Mutex<Queues>, wake: Arc<Notify>, running: Arc<AtomicBool> }` (`Drop` notifies),
  `Queues { evaluations: Vec<Value>, errors: Vec<Value>, dropped: u64 }`.
- `track_evaluation(&self, flag_key, result, variation: Option<&str>, environment: Option<&str>)`
  and `track_error(&self, report: ErrorReport)`: no-op if disabled; push under the lock (drop and
  count when at the bound); if a queue reached 100, `wake.notify_one()`; `ensure_flusher()`.
- `ensure_flusher`: if `Handle::try_current()` is `Ok` and `running` flips false→true, spawn a
  loop: `select!` on a 5 s sleep and `wake.notified()`; upgrade the `Weak<Inner>` or exit; flush;
  drop the strong reference before waiting again.
- `flush(&self)`: swap both queues out under the lock (never held across an await), then POST
  chunks to `<base_url>/api/telemetry/evaluations` (`{"evaluations": [...]}`) and `/errors`
  (`{"errors": [...]}`), URL built from path segments like `evaluate_url`.
- The client's `reqwest::Client` (with `Config::timeout`) is shared.

`FlagClient`: holds a `Telemetry`; `evaluate` calls `track_evaluation` on the cached and fresh
paths just before returning; `track_error` and `flush` delegate. The key check becomes
`starts_with("sdk_") || starts_with("srv_")`.

Public API (all additive): `FlagClient::track_error`, `FlagClient::flush`, `ErrorReport` (+
builders, re-exported), `Config::with_telemetry`. `OttoFlagsError::InvalidApiKey` keeps its name;
its message changes.

## 7. Invariants impact

- Never throw on a lookup: telemetry is fire-and-forget; lookups are unchanged in result.
- SDK REST contract: uses the documented telemetry endpoints, sending only fields the server
  reads (the guide's example omits `variation`, which the server accepts).
- Evaluations are counted, not stored, and no end-user context persisted: the SDK sends no
  `user_id`/context; error text is the host's own and the server truncates and expires it.
- Client keys public / server key secret: the key travels only in the `Authorization` header to
  `base_url`; it is never logged.
- Small shared database: batching (5 s / 100 events / 1 000 per request) keeps request volume to
  about one request per kind per 5 s per process.
- Licensing: MIT; no server code.

## 8. Migration

None.

## 9. Out-of-band

None. SDK-only plus its example: post-merge verification is server health only; not published.

## 10. Error handling and edge cases

- No runtime when an event is queued (e.g. `track_error` called from sync code before any async
  use): the event waits for the next `flush()` or for a later call made inside a runtime.
- Lock poisoned: events are dropped silently (as the cache does).
- Shutdown without `flush()`: up to 5 s of events lost; documented.
- Text truncation keeps whole UTF-8 characters.

## 11. Testing approach

Gates for `packages/rust-server/**` and `examples/rust-server/**`: fmt, clippy, test in both
crates. New `tests/telemetry_test.rs` (wiremock) covering each success criterion; unit tests in
`src/telemetry.rs` for truncation and the queue bound; a test in `tests/integration_test.rs` for
`srv_` acceptance and the error message; the example's `Settings` test for a `srv_` key.

## 12. Risks and open questions

- Every answered lookup now adds a queue push (a mutex lock and a small JSON value). Negligible
  next to the lookup itself.
- Existing wiremock tests do not mount telemetry routes; telemetry posts there would 404 and be
  logged and dropped, but tests finish before the 5 s flush and stay under 100 events.
