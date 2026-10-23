# Rust SDK Flag-Change Stream Consumer Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The Rust SDK reads `GET /api/flags/stream` when `enable_realtime` is on and invalidates cached evaluations on flag changes.

**Architecture:** A new `realtime` module owns an SSE parser, a reconnect-delay function and one background Tokio task per client family (started like the telemetry flusher). `FlagClient` starts it; events call `Cache::invalidate`.

**Tech Stack:** Rust 2021, tokio, reqwest 0.11 (`Response::chunk`), wiremock (tests). No new dependency.

**Ticket:** savvagent/otto-flags#167 (Rust only; PR body says `Refs #167`)

**Spec:** docs/specs/2026-10-23-rust-sdk-sse-consumer-design.md

## Global Constraints

- Gates, run from the worktree root `/home/robhicks/dev/otto-flags/.claude/worktrees/feat-167-rust-sse`:
  - `cd packages/rust-server && cargo fmt --check`
  - `cd packages/rust-server && cargo clippy --locked --all-targets -- -D warnings`
  - `cd packages/rust-server && cargo test --locked`
  - Task 3 only (touches `crates/`): `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`
  - Task 3 also: `podman build -t otto-flags:check .`
  - `/tmp` is tmpfs and breaks doc-test linking: if doc tests fail to link, set `TMPDIR` to a directory under the home dir.
- Evidence: report pass counts per test binary; a skipped or ignored test is not a pass.
- Invariants: the SDK never throws into the host app and a lookup never waits on the stream; the key goes only in the `Authorization` header and is never logged; no server code copied into the MIT package; public interface additive only; no version bump.
- Worktree only; trunk changes only through the PR
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends `Refs #167`; no attribution of any kind (no Co-Authored-By, no "Generated with", no credit markers)

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `packages/rust-server/src/realtime.rs` | `SseParser`, `reconnect_delay`, `Realtime` (task lifecycle, connect loop, event handling) |
| **Create.** `packages/rust-server/tests/realtime_test.rs` | Integration tests against wiremock |
| **Modify.** `packages/rust-server/src/lib.rs` | `mod realtime;`, crate docs feature bullet |
| **Modify.** `packages/rust-server/src/types.rs` | `Config::with_realtime(bool)` |
| **Modify.** `packages/rust-server/src/client.rs` | field `realtime`, start in `new` and `evaluate`, make `is_valid_flag_key` reachable (already `pub(crate)`) |
| **Modify.** `packages/rust-server/README.md` | document real-time behaviour and `with_realtime` |
| **Modify.** `crates/flags-mcp/content/guides/rust.md` | one paragraph on real-time behaviour |

## Task Order & Rationale

Task 1 builds the pure, unit-tested pieces (parser, delay) and the task machinery against the cache. Task 2 wires it into the client and proves behaviour end to end. Task 3 is docs and the only `crates/` change, so it carries the workspace gates and the container build.

---

### Task 1: realtime module (parser, delay, stream task)

**Files:**
- Create: `packages/rust-server/src/realtime.rs`
- Modify: `packages/rust-server/src/lib.rs` (add `mod realtime;`)

**Interfaces:** produces `pub(crate) struct Realtime` with `Realtime::new() -> Self` (Clone), `Realtime::ensure_started(&self, target: &StreamTarget)`, and `pub(crate) struct StreamTarget { pub cache: Cache, pub http: reqwest::Client, pub url: reqwest::Url, pub api_key: String }` (Clone); `pub(crate) fn stream_url(base_url: &str) -> Option<reqwest::Url>` (`<base>/api/flags/stream`, path prefix kept, as `client.rs` `evaluate_url` does); `SseParser`; `reconnect_delay`. Task 2 consumes them.

**Out-of-band / invariants:** none. Never log `api_key`; stream failures never touch `Backoff`.

**Model:** opus — async task lifecycle and cancellation; the timing rules need judgement.

Behaviour to implement (all of it from the spec's Architecture and Error handling sections):

```rust
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
const BASE_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);
const REJECTED_MAX_DELAY: Duration = Duration::from_secs(300);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const STABLE_AFTER: Duration = Duration::from_secs(30);

pub(crate) struct SseEvent { pub event: String, pub data: String }
pub(crate) struct TooLarge;
#[derive(Default)] pub(crate) struct SseParser { /* partial line bytes, current event name, data lines */ }
impl SseParser { pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, TooLarge>; }

/// min(BASE * 2^attempt, cap), scaled into [50%, 100%] by `unit` in [0, 1).
pub(crate) fn reconnect_delay(attempt: u32, cap: Duration, unit: f64) -> Duration;
fn unit_random() -> f64; // from std::hash::RandomState (BuildHasher::build_hasher().finish() / u64::MAX as f64), clamped below 1.0
```

SSE rules: lines end at `\n`, a trailing `\r` is dropped; a line starting with `:` is a comment; `event:`/`data:` fields strip one leading space after the colon; multiple `data` lines join with `\n`; an empty line dispatches the pending event (event name defaults to `message`; an event with no data lines and no name is dropped); `TooLarge` when the pending line plus accumulated data exceeds `MAX_EVENT_BYTES`. A chunk may split anywhere, including inside a UTF-8 character: buffer bytes, decode a line only once complete (`String::from_utf8_lossy`).

Task loop (one `async fn run(weak: Weak<Inner>, target: StreamTarget)`, spawned by `ensure_started` with the `running` / `ResetOnDrop` / `Weak<Inner>` / `Notify` pattern of `telemetry.rs` `ensure_flusher`): upgrade the `Weak` at the top of each iteration and stop if gone; connect with `GET`, headers `Authorization: Bearer <key>` and `Accept: text/event-stream`; on shutdown: the connect `send()` and every `chunk()` read are `select!`ed against the shutdown `Notify`, so dropping the last client closes an open stream at once (heartbeats keep the idle timeout from ever firing); on a non-2xx status choose the delay (`REJECTED_MAX_DELAY` cap for 401/403/404, `error` log for 401 else `warn`; for 429/503 take `max(delay, min(Retry-After, MAX_RETRY_AFTER))` using `backoff::parse_retry_after`); on 2xx read `chunk()` under `tokio::time::timeout(IDLE_TIMEOUT, ...)`, feed the parser, and for each event: `connected` -> `cache.invalidate(None)`; `flag.created|updated|deleted` -> parse `data` as JSON, take string field `key`, if `is_valid_flag_key(key)` -> `cache.invalidate(Some(key))`; anything else ignored. When the connection ends (EOF, error, idle timeout, `TooLarge`): if it was up at least `STABLE_AFTER` reset the attempt counter to 0, else increment; sleep `reconnect_delay(attempt, cap, unit_random())` selecting against the shutdown `Notify`. `Inner::drop` calls `notify_one`. The first retry waits `reconnect_delay(0, ..)` (0.5 to 1 s): compute the delay from the counter before incrementing it. `ensure_started` does nothing without a current runtime (`Handle::try_current`) and does nothing if `running` is already set.

- [ ] **Step 1: Write the failing unit tests** in `#[cfg(test)] mod tests` of `realtime.rs` (module declared in `lib.rs`; stub the items with `todo!()` so it compiles):

```rust
#[test]
fn parser_splits_events_across_arbitrary_chunk_boundaries() {
    let wire = "event: connected\ndata: {\"a\":1}\n\n: comment\nevent: flag.updated\r\ndata: {\"key\":\"f\"}\r\n\r\n";
    for split in 0..=wire.len() {
        let mut p = SseParser::default();
        let mut got = p.feed(&wire.as_bytes()[..split]).unwrap();
        got.extend(p.feed(&wire.as_bytes()[split..]).unwrap());
        let got: Vec<_> = got.into_iter().map(|e| (e.event, e.data)).collect();
        assert_eq!(got, vec![
            ("connected".to_string(), "{\"a\":1}".to_string()),
            ("flag.updated".to_string(), "{\"key\":\"f\"}".to_string()),
        ], "split at {split}");
    }
}

#[test]
fn parser_joins_data_lines_and_defaults_the_event_name() {
    let mut p = SseParser::default();
    let got = p.feed(b"data: a\ndata:b\n\n").unwrap();
    assert_eq!((got[0].event.as_str(), got[0].data.as_str()), ("message", "a\nb"));
}

#[test]
fn parser_keeps_a_split_multibyte_character_intact() {
    let wire = "data: \u{e9}\n\n".as_bytes();
    let mut p = SseParser::default();
    let mut got = p.feed(&wire[..7]).unwrap();
    got.extend(p.feed(&wire[7..]).unwrap());
    assert_eq!(got[0].data, "\u{e9}");
}

#[test]
fn parser_refuses_an_event_over_the_cap() {
    let mut p = SseParser::default();
    let big = vec![b'x'; MAX_EVENT_BYTES + 1];
    assert!(p.feed(&big).is_err());
}

#[test]
fn reconnect_delay_doubles_to_the_cap_with_jitter_in_the_upper_half() {
    let s = Duration::from_secs;
    assert!(reconnect_delay(0, MAX_DELAY, 0.0) == Duration::from_millis(500));
    assert!(reconnect_delay(0, MAX_DELAY, 0.999999) < s(1));
    assert!(reconnect_delay(3, MAX_DELAY, 0.0) == s(4));
    assert!(reconnect_delay(30, MAX_DELAY, 0.0) == s(15));
    assert!(reconnect_delay(30, REJECTED_MAX_DELAY, 0.0) == s(150));
    assert!(reconnect_delay(u32::MAX, MAX_DELAY, 0.5) <= MAX_DELAY);
}

#[test]
fn stream_url_keeps_a_path_prefix_and_rejects_garbage() {
    assert_eq!(stream_url("https://h.example/p/").unwrap().as_str(), "https://h.example/p/api/flags/stream");
    assert!(stream_url("not a url").is_none());
}
```

Add a unit test beside `telemetry.rs`'s equivalent: start the task against a wiremock stream whose response is delayed 30 s, drop every `Realtime` clone and the `Inner`, and assert `running` becomes false within 2 s.

- [ ] **Step 2: Run to verify failure.** Run: `cd packages/rust-server && cargo test --locked --lib realtime`. Expected: FAIL (`not yet implemented`).
- [ ] **Step 3: Implement** `SseParser`, `reconnect_delay`, `unit_random`, `stream_url`, `StreamTarget`, `Realtime`, `run` as described above.
- [ ] **Step 4: Run to verify pass.** Run the three package gates (fmt, clippy, test). Expected: PASS, none skipped. Nothing outside tests uses the module until Task 2, so put `#![cfg_attr(not(test), allow(dead_code))]` at the top of `realtime.rs` for this task and remove it in Task 2 (clippy `-D warnings` fails on dead code otherwise).
- [ ] **Step 5: Commit.**

```bash
git add packages/rust-server/src/realtime.rs packages/rust-server/src/lib.rs
git commit -m "feat(sdk): add the Rust SDK flag-change stream task" -m "The stream must invalidate cached evaluations so a kill switch does not wait for the cache TTL.

Refs #167"
```

---

### Task 2: wire it into the client and prove it end to end

**Files:**
- Modify: `packages/rust-server/src/realtime.rs` (remove the temporary `allow(dead_code)`)
- Modify: `packages/rust-server/src/types.rs` (add `with_realtime`, after `with_telemetry`)
- Modify: `packages/rust-server/src/client.rs`
- Create: `packages/rust-server/tests/realtime_test.rs`

**Interfaces:** consumes `Realtime`, `StreamTarget`, `stream_url` from Task 1.

**Out-of-band / invariants:** additive public API (`Config::with_realtime`) only; `evaluate` adds one atomic check, no I/O.

**Model:** sonnet — contained wiring plus tests the plan spells out.

Wiring: `FlagClient` gets `realtime: Realtime` and `stream: Option<StreamTarget>`. In `new`: when `config.enable_realtime`, build a second `reqwest::Client` with `.connect_timeout(config.timeout)` and no total timeout, compute `stream_url(&config.base_url)`; on `None` log one `warn` ("Otto Flags: base_url is not a valid URL; real-time updates are off") and set `stream = None`; else `stream = Some(StreamTarget { cache: cache.clone(), http, url, api_key })`, then call `ensure_started` if present. In `evaluate`, first line after the key check: `if let Some(t) = &self.stream { self.realtime.ensure_started(t); }`. `with_realtime(mut self, enabled: bool) -> Self` sets `enable_realtime` (doc: "on by default").

- [ ] **Step 1: Write the failing integration tests** in `tests/realtime_test.rs`. Every test config uses `.with_telemetry(false)` (no telemetry POSTs to unmocked paths). Helpers: `KEY = "sdk_test_key"`; `mock_evaluate(server, flag, enabled)` (POST `/api/flags/{flag}/evaluate`, bearer header, body `{"key":flag,"enabled":enabled}`; for a changed answer mount a second mock with `.with_priority(1)` and the first with `.with_priority(5)`); `mock_stream(server, body, delay)` (GET `/api/flags/stream`, `set_body_raw(body, "text/event-stream")`, `.set_delay(delay)`); `stream_requests(server)` counts received GETs to that path; `wait_until(timeout, f)` polls every 25 ms. Tests (all `#[tokio::test]`):

1. `an_update_event_drops_the_cached_evaluation`: evaluate mock answers `true`; stream mock `set_delay(700ms)` with body (no `connected` event, which would clear the cache itself) `"event: flag.updated\ndata: {\"key\":\"f\"}\n\n"`; client with a long `cache_ttl` (10 min); `evaluate("f")` gives `reason == "evaluated"`, a second gives `"cached"`; then re-mock evaluate to `false`; `wait_until(5s)` an `evaluate("f")` returns `value == false` and `reason == "evaluated"`. (The assertion proves the event, not the TTL, ended the cache entry.)
2. `a_dropped_stream_is_reconnected_and_the_cache_is_cleared`: stream mock with body `"event: connected\ndata: {}\n\n"`, `set_delay(800ms)` (the server closes after the body); `cache_ttl` 10 min; evaluate `g` twice immediately (second `"cached"`, before the first `connected` lands); `wait_until(8s)` `stream_requests >= 2`; then `wait_until(3s)` an `evaluate("g")` returns reason `"evaluated"` (each `connected` cleared the cache, so a read after the reconnect re-fetches).
3. `lookups_are_served_and_expire_by_ttl_while_the_stream_is_down`: stream mock `ResponseTemplate::new(503)`; `cache_ttl(300ms)`; evaluate -> `"evaluated"`, again -> `"cached"`, sleep 400 ms, evaluate -> `"evaluated"`; `is_enabled` never panics; no `Err`.
4. `no_stream_is_opened_when_realtime_is_off`: `Config::...with_realtime(false)`; evaluate once; sleep 1500 ms; `stream_requests == 0`.
5. `a_lookup_does_not_wait_for_the_stream`: stream mock `set_delay(30s)`; `tokio::time::timeout(2s, client.evaluate("f"))` is `Ok`; after it, `stream_requests >= 1` eventually (the stream was in fact attempted).
6. `the_stream_request_carries_the_key_in_the_header_only`: `wait_until(3s)` a received GET to `/api/flags/stream` has header `authorization == "Bearer sdk_test_key"`, `accept` containing `text/event-stream`, and its URL query is empty.
7. `a_delete_event_for_another_flag_leaves_this_one_cached` and `an_event_with_an_invalid_key_is_ignored`: one test: stream mock `set_delay(500ms)`, body with `flag.deleted` for key `other` and `flag.updated` for key `a/b` and no `connected`; cache `f` first (long TTL), wait past the delivery (stream request seen, then 700 ms), `f` is still `"cached"`. The invalid-key filter itself is covered by a unit test in `realtime.rs` on a helper `invalidation_for(event: &SseEvent) -> Option<Invalidation>` (`Invalidation::All | Flag(String)`) that `run` uses: `flag.updated` with key `a/b` and malformed JSON give `None`, `connected` gives `All`, `flag.created` with key `f` gives `Flag("f")`.

- [ ] **Step 2: Run to verify failure.** Run: `cd packages/rust-server && cargo test --locked --test realtime_test`. Expected: FAIL (compile error on `with_realtime`, then assertion failures).
- [ ] **Step 3: Implement the wiring** (types.rs, client.rs).
- [ ] **Step 4: Run the three package gates.** Expected: PASS, no test ignored. Run `cargo test --locked --test realtime_test` three times in a row to check for flakes.
- [ ] **Step 5: Commit.**

```bash
git add packages/rust-server/src packages/rust-server/tests/realtime_test.rs
git commit -m "feat(sdk): consume the flag-change stream in the Rust SDK" -m "enable_realtime was documented and on by default but read nowhere, so a kill switch waited for the cache TTL. The client now reads the stream and drops a changed flag's cached results.

Refs #167"
```

---

### Task 3: documentation and `sdk_guide` text

**Files:**
- Modify: `packages/rust-server/README.md`, `packages/rust-server/src/lib.rs` (feature bullet in the crate docs)
- Modify: `crates/flags-mcp/content/guides/rust.md`

**Interfaces:** none.

**Out-of-band / invariants:** the `crates/` change deploys with the server on merge; no secret, scope or config key. The guide never contains the literal server-key prefix (a guide test asserts it; keep that).

**Model:** haiku — documentation matched to a finished change.

Content to state, accurately: real-time is on by default (`with_realtime(false)` turns it off); a changed flag's cached results are dropped within about a round trip; the stream connects in the background (at client creation inside a Tokio runtime, otherwise on the first `evaluate`) and never delays or fails a lookup; while it is down, lookups use `cache_ttl` as before; it reconnects with exponential backoff (1 s up to 30 s, 5 min if the key is rejected) and clears the whole cache on every (re)connection; one stream per client (clones share it); the stream is one long-lived connection to the server per process.

- [ ] **Step 1: Edit the three files.** In the README add `.with_realtime(true) // the default` to the config builder example and a "Real-time updates" subsection; fix the `invalidate_cache` text if it implies TTL-only.
- [ ] **Step 2: Run the gates.** `cd packages/rust-server && cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked`; then `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `podman build -t otto-flags:check .`. Expected: all pass.
- [ ] **Step 3: Commit.**

```bash
git add packages/rust-server/README.md packages/rust-server/src/lib.rs crates/flags-mcp/content/guides/rust.md
git commit -m "docs(sdk): describe the Rust SDK's real-time updates" -m "The README, crate docs and sdk_guide text did not mention the stream the SDK now reads.

Refs #167"
```
