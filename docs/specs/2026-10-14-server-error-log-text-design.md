# Server failure logging outside flags-mcp: a reason above debug, the text only at debug

**Status:** DRAFT
**Issue:** #35

## 1. Brief

From #35 (follow-up to #29, which fixed `flags-mcp` only):

> Outside `flags-mcp`, failures are still logged at `warn` or `error` with their full text
> (`error = %e`). Database, platform, and listener errors can carry SQL, schema names, platform
> response bodies, and URLs. The rule #29 applies, and #24's rollback checker already follows, is:
> log the stable error code above `debug`, and the text only at `debug`.
>
> Sites on `main` at bf4e43c:
>
> - `crates/flags-api/src/error.rs:52` (`SDK request failed`, error)
> - `crates/flags-api/src/platform.rs:44` (`platform webhook rejected`, warn) and `:51` (`could not
>   apply a platform event`, error)
> - `crates/flags-core/src/notify.rs:95, 103, 114, 117` (flag-change listener, warn)
> - `crates/flags-core/src/usage.rs:350, 384, 394` (usage shipping, warn)
> - `crates/flags-server/src/main.rs:105, 109, 130, 135` (shutdown and sweeps, warn)
> - `crates/flags-server/src/health.rs:65` (`readiness check failed`, warn)
>
> **Acceptance criteria**
> - [ ] Each site logs a text-free code (or variant kind) at its current level, and the error text
>   only at `debug`.
> - [ ] HTTP responses and webhook status codes are unchanged.
> - [ ] `grep -rnE 'tracing::(error|warn|info)!\([^)]*error = %' crates/` finds nothing.
>
> **Out of scope:** `flags-mcp` (#29).

## 2. Premise corrections

The issue's grep matches only calls written on one line with an `error = %` field. Three more
sites log database or platform text above debug under another field name or across lines; they
are the same defect and are in scope:

- `crates/flags-server/src/main.rs:158` (`check_platform`): `tracing::warn!(platform = …, error =
  %e, …)` on an `otto_resource::Error`, wrapped across lines by rustfmt.
- `crates/flags-core/src/usage.rs:155` (quota lookup fail-open): `reason = %failure`, where
  `failure` is the platform error's `to_string()` (`Status` bodies, transport URLs).
- `crates/flags-api/src/platform.rs:38` (`platform webhook was signed but unreadable`, error):
  `%reason` is serde's text for the body, which can quote body values.

Not changed: `usage.rs:314` logs the platform's per-event rejection `reason`, a field the
platform's usage API defines for operators and that is stored in `usage_outbox_rejected`; it is
the product of a successful exchange, not error text. `entitlements.rs:142` and `rollback.rs`
already follow the rule.

The `serde_json` error at `notify.rs:114` and the `JoinError`s at `main.rs:105, 109` carry no
database or platform text, but the issue lists them and the rule is uniform: they get a kind too.

## 3. Assumptions

- **Reuse #29's vocabulary.** `flags_core::Error::failure_reason()` and
  `flags_core::error::platform_failure_reason()` already exist. The sqlx-level helper behind them,
  `db_failure_reason`, becomes `pub` so sites holding a bare `sqlx::Error` (notify, health, the
  usage outbox) log the same names.
- **`ShipError::failure_reason()`** in `usage.rs` delegates: `Platform` → `platform_failure_reason`,
  `Db` → `db_failure_reason`.
- **Webhook rejection kinds** are a private `fn rejection_reason(&WebhookError) -> &'static str` in
  `flags-api/src/platform.rs`: `malformed_header`, `bad_signature`, `stale`, `bad_body`.
- **Other kinds:** a `JoinError` logs `reason = "panicked"` or `"cancelled"`; an unreadable
  flag-change payload logs `reason = "decode"`; a quota-lookup timeout logs `reason = "timeout"`
  (as text it was `"timed out"`), the name `entitlements.rs` already logs for a timeout of the same
  platform call, so one filter finds both.
- **Field names follow #29:** `code` (the wire code, where the site has a `flags_core::Error`),
  `reason`, and `status` (HTTP status of a platform failure, when there is one). Sites holding
  only a `sqlx::Error` log `reason` alone.
- **Two events per site, inline,** as `rollback.rs` does: the existing level with the fields, then
  `tracing::debug!` with the same fields plus `error = %e` and the same message. No shared logging
  helper: the levels differ by site, and `tracing`'s level is a macro, so a helper would need one
  function per level for two lines each.
- **Test-only dependencies.** `tracing-subscriber` and `sqlx` (workspace) become `flags-api`
  dev-dependencies, for a log-capture test in its own binary (as #29's) that builds a
  `sqlx::Error` and a lazy pool. No production dependency changes.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

No database, platform, or listener error text is logged above `debug` anywhere in `crates/`;
operators still see at `info` which kind of failure happened.

- Every site in §1 and §2 logs at its current level with `reason` (and `code`/`status` where
  available) and no error text, and logs the text at `debug`.
- A log-capture test over `ApiError::from(flags_core::Error)` and the webhook route shows the
  lines above `DEBUG` carry `code`/`reason` and not the error's text, and the `DEBUG` line carries
  the text.
- A source-scan test fails on any `error!`/`warn!`/`info!` event in a workspace crate that carries
  a text field (`error = %`, `error = ?`, `reason = %`, `%reason`), wrapped calls included, with
  `usage.rs`'s `reason = %r.reason` as its one named exception.
- `ShipError::failure_reason`, `rejection_reason`, and `db_failure_reason` (through its existing
  coverage) are unit-tested.
- HTTP status codes and bodies for SDK requests, webhooks, and `/readyz` are unchanged; the
  existing e2e tests pin them.
- `grep -rnE 'tracing::(error|warn|info)!\([^)]*error = %' crates/` finds nothing (the issue's
  check), and so does this span-aware one, which also sees calls rustfmt wraps and the `reason =
  %` / `%reason` field forms; it may print only `usage.rs`'s `reason = %r.reason` (§2):

  ```sh
  find crates -name '*.rs' -print0 | xargs -0 perl -0ne 'while (/tracing::(error|warn|info)!\((.*?)"\s*,?\s*\)/sg) { my $b=$2; $b =~ s/\s+/ /g; print "$ARGV: $b\n" if $b =~ /error = %|reason = %|%reason/ }'
  ```

## 5. Scope

**In:** `crates/flags-api/src/{error,platform}.rs`, `crates/flags-api/tests/failure_logs.rs`,
`crates/flags-api/Cargo.toml` (dev-dependency), `Cargo.lock`, `crates/flags-core/src/{error,notify,
usage}.rs`, `crates/flags-server/src/{main,health}.rs`.

**Out:** `flags-mcp` (#29); the stored `usage_outbox.last_error` text (a database column, not a
log, read by no tool); the platform's rejection reason at `usage.rs:314`; log format and
subscriber configuration.

## 6. Architecture

- `flags-core` keeps owning what a failure is called: `db_failure_reason` goes public beside
  `platform_failure_reason`, and `ShipError` gains `failure_reason` next to its definition.
- Each site changes only its logging; control flow and return values are as before.
  - `flags-api/src/error.rs` `From<flags_core::Error>`: `error!(code, reason, status, "SDK request
    failed")`, `debug!(code, reason, error = %e, …)`.
  - `flags-api/src/platform.rs`: `BadBody` logs `reason = "bad_body"` at error, the serde text at
    debug; other rejections log `rejection_reason(&e)` at warn; an apply failure logs `code`,
    `reason`, `status`, and `event_id` at error, the text at debug.
  - `flags-core/src/notify.rs`: connect, `LISTEN`, and `recv` failures log
    `db_failure_reason(&e)`; a bad payload logs `"decode"`.
  - `flags-core/src/usage.rs`: `reschedule` logs `db_failure_reason`; the shipping loop and final
    flush log `e.failure_reason()`; the quota fail-open logs `platform_failure_reason` (or
    `timeout`) and its `status`.
  - `flags-server/src/main.rs`: `JoinError` kind (`is_panic()` → `panicked`, else `cancelled`,
    which covers a task aborted from outside; the shutdown path's own abort after the 5 s timeout
    is not joined, so it is not logged here); sweeps log `e.code()` and
    `e.failure_reason()` (for a non-database `flags_core::Error` the reason falls back to the code,
    so the two can match; intended);
    `check_platform` logs `platform_failure_reason`.
  - `flags-server/src/health.rs`: `db_failure_reason`.
- Public interface: `flags_core::error::db_failure_reason` and `ShipError::failure_reason` are
  additive. Nothing on the wire changes.

## 7. Invariants impact

- **Database error text never reaches a tool response:** unaffected; this extends the care to logs.
- **Platform webhooks signature-verified, fail closed:** the verification and status codes are
  unchanged; only what is logged.
- **SDK REST contract stable:** `ApiError` responses unchanged.
- No migration, SQL, tenant query, metering, config key, or scope change.

## 8. Migration

None.

## 9. Out-of-band

None. Deploy is CI-auto on merge. Production logs at `info` (`fly.toml` `RUST_LOG`), so the text
is no longer written there; raising a crate to `debug` brings it back for diagnosis.

## 10. Error handling and edge cases

- `sqlx::Error` is `#[non_exhaustive]`; `db_failure_reason` already maps unknown variants to
  `other`. `WebhookError` is matched exhaustively, so a new variant fails the build until named.
- A `JoinError` that is neither panic nor cancel does not exist in tokio; `is_panic()` decides.
- At `debug` both events appear, so local debugging loses nothing.

## 11. Testing approach

- `crates/flags-api/tests/failure_logs.rs`: one test, own binary (the callsite-interest caching
  #29 hit). Cases: `Error::Db` with a marker text (`Protocol("MARKER_DB")`) and
  `Error::Platform(Status { status: 503, body: "MARKER_PLATFORM" })`, each answering 503 with no
  marker in the message; a signed webhook body serde rejects (its text quotes the rejected value;
  400) and a forged signature (401), both through the router on a lazy pool that is never used.
  Assert the lines above `DEBUG` carry the expected `code=`/`reason=`/`status=` and no marker, and
  the `DEBUG` lines carry the marker.
- Unit tests: `ShipError::failure_reason` (both variants, no text) in `usage.rs`;
  `rejection_reason` for each `WebhookError` variant in `platform.rs`; `join_failure_reason` (a
  real panic and a real abort) in `main.rs`.
- `crates/flags-server/tests/log_text.rs`: the source scan in §4, plus a test of the scanner on a
  wrapped call.
- The other sites (listener, readiness, sweeps, shutdown, startup platform check) are reached only
  through a failing database or platform at runtime; their reasons come from the unit-tested
  helpers, and the source-scan test guards against text fields coming back. No test drives their
  failure: forcing those failures costs more than it proves.
- Gates: `cargo test --workspace` with `DATABASE_URL`, clippy, fmt, the migrations check (vacuous),
  and `podman build` (crates/ changed).

## 12. Risks and open questions

- Dashboards or alerts keyed on the old `error` text at warn: none known as of 2026-10-14 (logs
  go only to `fly logs`).
- The quota fail-open `reason` value changes form (text → kind); it is read by no tool.
