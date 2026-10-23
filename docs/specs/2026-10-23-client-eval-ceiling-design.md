# Bound what a public client key can add to an org's bill

**Status:** DRAFT
**Ticket:** savvagent/otto-flags#143 (PR 1 of 2; PR 2 adds a per-app origin allowlist)

## Brief

Ticket #143, "Limit how far a public client key can raise an org's metered evaluations", verbatim
(untrusted text, quoted as data):

> After #140, anyone holding an app's public client key (`sdk_`) can call `POST /api/evaluate/{key}`
> repeatedly. Each answered evaluation is billed to the org (`served`). Client-key telemetry is not
> billed, so faked reports can't raise a bill. Repeated evaluate calls still can, at the cost of one
> request per evaluation. The budget's opt-in hard stop caps the damage once it trips, but nothing
> limits the rate.

The decision, made with the owner (Rob) on 2026-10-23, after the meter was settled as evaluations
(#142, closed):

- Browser and mobile clients are the biggest consumers, so client-key evaluations must stay billed.
  Leaving them unbilled, or billing only server keys, would give most of the revenue away.
- A public key cannot be made trustworthy, so the exposure is bounded rather than removed: a
  per-app hourly ceiling on the billed client-key quantity, an origin allowlist (PR 2), a split of
  client-served and server-served counts in `usage` with a way to notice a spike, and a check that
  only the app's own environments are billed.

## Premise corrections

- #143's comment says evaluations "in every environment are billed, not only `production`". That is
  what the code did: `evaluate` counted every answer whatever environment the caller named, while
  `CLAUDE.md` and the #140 spec say "production evaluations". The spec's intent (the app's own
  environments) is what this PR enforces.
- "A cap that cuts service off" is not what is built. A budget never turns flags off (#140), so
  evaluations above the ceiling are answered; only their billing stops.
- No hourly traffic exists in production yet (`usage_meter_hourly`, `usage_outbox` and
  `flag_eval_hourly` are empty, two apps), so the default ceiling is sized from capacity, not from
  observed use (see Assumptions).

## Decisions

- **Billed quantity per (org, app, hour)** becomes
  `GREATEST((served - served_client) + LEAST(served_client, ceiling), reported)`.
  - `served_client` is the part of `served` answered to a public client key; the rest of `served`
    (server keys) is unchanged. `reported` (server-key telemetry) is unchanged, so a server SDK's
    telemetry is never clipped.
  - The ceiling is one server-wide value, `FLAGS_CLIENT_EVAL_CEILING_PER_HOUR`, applied when the
    hour closes. Changing it affects hours that close afterwards.
  - Evaluations above the ceiling are answered exactly as before; only their billing stops. The
    request path is unchanged: still one in-memory counter increment, no platform call, no extra
    connection checkout.
- **`clipped`** for an hour is `GREATEST(served, reported)` minus the ceiled quantity: what the
  ceiling actually left unbilled (zero when telemetry already bills the hour).
- **Only the app's own environments are counted.** `evaluate` reads the app inside the transaction
  it already opens for the flag (one primary-key read, same connection) and counts the answer only
  when the requested environment is one of `FlagApp::environments`. An unknown environment is
  answered as today and not counted. Server-key telemetry (`reported`) is not filtered: the server
  key is secret.
- **Persistence for the split:** `usage_meter_daily (org_id, app_id, day, served_client,
  served_server, clipped)`, written by the same statement that closes an hour (the closing
  `DELETE ... RETURNING` takes each hour exactly once, so the day's totals are counted once).
  It is swept at the 90-day evaluation retention, purged with the org, tenant-isolated like
  `usage_meter_hourly`, and at most (apps x 90) rows per org. Blocked orgs' hours are recorded too.
- **Surfacing:** the MCP `usage` tool gains `evaluations`: this month's `clientServed`,
  `serverServed` and `clipped` (UTC month, closed hours only) and the 20 busiest apps. When a close
  pass clips an hour it logs a WARN per clipped hour (the 20 largest per pass: org, app, hour,
  `served_client`, `ceiling`, `clipped`) and its summary line carries `clipped_hours` and
  `clipped_evaluations`.
- **Migration `0010_client_eval_ceiling.sql`** is additive: `usage_meter_hourly.served_client
  bigint NOT NULL DEFAULT 0 CHECK (served_client >= 0 AND served_client <= served)` and the new
  table. The previous release inserts and updates `usage_meter_hourly` without the column (default
  0; the check holds because it only raises `served`), and never reads `usage_meter_daily`.
  Hours it closes during a rollout overlap bill under the old formula and record no split.

## Assumptions

- **Default ceiling 500000 per app per hour** (about 139 a second sustained). The shared instance
  (512 MB, 5 pool connections) cannot answer much more than that in an hour, so the default is
  above anything one app can legitimately do here and below what a hostile loop could bill without
  a limit. It is a pricing decision: Rob sets the real number with the prices, as an env var in
  `fly.toml`, without a code change.
- The ceiling is per app, not per key kind or per origin; per-app overrides are a follow-up.
- `usage` reports the UTC calendar month. The platform's month boundary and its 2 h cutoff for late
  hours (`docs/deploy/fly.md`, Metering) can differ at the edges.

## Risks

- A ceiling bounds the inflation, it does not prevent it: an attacker can still bill an org up to
  the ceiling every hour. The remedies are the `usage` split (spot a client-served spike), key
  rotation (`rotate_app_keys`) and a credit; PR 2 adds the origin allowlist.
- An app with legitimate client traffic above the ceiling is under-billed. The WARN and `usage`
  make that visible so the ceiling can be raised.

## Non-goals

- Per-app ceiling overrides settable through `update_app` (needs an owner/admin check); follow-up.
- The origin allowlist (PR 2 of #143).
- Short-lived client tokens minted by the org's backend (changes every SDK).
- Changing the `sdk_guide` recipe text, `SDK_REF`, or any SDK: no SDK or REST-contract change.

## Success criteria

- A client-key hour above the ceiling bills `(served - served_client) + ceiling` (or `reported`, if
  larger) and records the rest as `clipped`.
- An evaluation in an environment the app does not have is answered and counted nowhere.
- `usage` shows this month's client/server split and the clipped quantity per app.
- The previous release boots against the new schema; `migrations-append-only` is clean.
