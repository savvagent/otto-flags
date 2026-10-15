# Keep flag_health's lists cheap and uncrowdable

**Status:** DRAFT
**Issues:** #69, #70

## 1. Brief

From #69 (found by the database review of #65 (#59); the behaviour predates both):

> `flag_health`'s error-group query (`crates/flags-core/src/telemetry.rs`, `health`) takes each
> group's latest message with `(array_agg(error_message ORDER BY occurred_at DESC))[1]`. That
> builds an array of every message in the group (up to 2,000 characters each) just to keep one.
> The `LIMIT` on the groups does not help, because every group is aggregated before the cut. A
> public `sdk_` client key can report any number of 1,000-event batches, so over the 14-day error
> retention one error type can grow an array large enough to put the 512 MB shared instance under
> memory pressure, and eventually fail `flag_health` at Postgres's 1 GB array limit.
>
> **Acceptance criteria**
> - [ ] The error-group query no longer aggregates every message per group; for example, aggregate
>   counts and `max(occurred_at)` first, then fetch the newest message only for the groups that
>   survive the `LIMIT` (a `LATERAL` lookup through `flag_errors_flag_time_idx`).
> - [ ] `top_errors` output is unchanged (same counts, same latest message).
> - [ ] A test covers the latest-message choice.

From #70 (found by the security and architecture reviews of #65 (#59)):

> Since #65, `flag_health` shows the 20 most evaluated variation names and the 10 most frequent
> error groups (merged after cleaning, from the 100 most frequent raw groups). Both are ranked
> purely by count, and anyone holding an app's public `sdk_` client key can add counts:
>
> - 20 made-up variation names with more evaluations than the flag's real variations push the real
>   ones out of `variations` into the `otherVariations` sum, hiding how traffic splits across the
>   flag's own variations.
> - More than 100 frequent raw spellings of one junk error type fill the fetch behind
>   `top_errors`, keeping rarer real error types out (the junk then shows as one merged row).
>
> This only hides information; it changes no flag, and the provenance note tells agents the counts
> are untrusted. `flag_errors` and the rollups already record the reporter (server or client key).
>
> **Acceptance criteria**
> - [ ] A flag's declared variations are always shown in `variations` (undeclared names fill any
>   remaining slots, or are reported separately).
> - [ ] `top_errors` cannot be emptied of server-key-reported errors by client-key reports (for
>   example, rank or split by reporter, as auto-rollback defaults to server-key reports).
> - [ ] Tests cover both.

Both change the same function (`telemetry::health`) and its MCP output, so they ship in one PR.

## 2. Premise corrections

- **#69's "output unchanged" and #70's split by reporter meet in one PR.** The query rewrite for
  #69 alone keeps every group's count and latest message (tested on single-reporter groups). #70
  then deliberately splits a type reported through both key kinds into two groups, one per
  reporter. So against `main`, `top_errors` changes only by that split and the new `reporter`
  field; within each group, counts and latest message are what the old query gave.
- **"Latest message" was not fully defined.** `array_agg(... ORDER BY occurred_at DESC)[1]` picks
  an arbitrary row among several with the same `occurred_at` (a batch from one SDK often shares a
  timestamp). The new lookup breaks that tie by the highest `id` (the row stored last), so the
  answer is deterministic. Where `occurred_at` differs, the choice is the same as before.
- **The lookup can seek one timestamp, not walk the flag's history.** The issue suggests a
  `LATERAL` lookup through `flag_errors_flag_time_idx (flag_id, occurred_at DESC)`. Ordered by
  `occurred_at DESC` and filtered by type, that lookup walks every newer row of other groups before
  reaching a rare group's newest row. The group's `max(occurred_at)` is already known, so the
  lookup matches `occurred_at = last_seen` exactly: an index equality seek to the rows at that
  instant, then filtered by type, state, reporter, and environment.

## 3. Assumptions

- **Declared variations are shown even at zero.** "Always shown" is read literally: a declared
  variation no one evaluated in the window appears with `0`, which is itself the answer to "how
  does traffic split". Undeclared names fill the remaining `SHOWN_VARIATIONS - declared` slots by
  count (ties by name), as today. A flag may declare at most 20 (`eval::MAX_VARIATIONS`), the
  number shown, so declared names always fit; a flag declaring 20 shows no undeclared names
  (they are all in `otherVariations`).
- **Declared names are fetched first, in the same statement.** The variations query orders
  declared names ahead of the rest (`ORDER BY variation = ANY($6) DESC, 2 DESC, variation`) before
  its `LIMIT 100`, so at most 20 declared rows are always within the fetch, and the window total
  keeps coming from the same snapshot as the rows (the #65 decision). No second statement.
- **Declared names come from the flag `health` is asked about.** `health` takes the
  `&FeatureFlag` (it already took its id; the MCP tool already loaded the flag) and reads the keys
  of `flag.variations`. Declared names pass `validate_name` (ASCII letters, digits, `-_.`, 1-64),
  so `inert` leaves them unchanged and they never collide with each other. A reported undeclared
  raw name that cleans to a declared name is counted in that declared entry (it reads alike; the
  map cannot hold two equal keys anyway).
- **Error groups are split by reporter, and server groups keep half the slots.** Grouping becomes
  `(error_type, flag_enabled, reporter)`, with `reporter` NULL (rows from before 0002) counted as
  client, as auto-rollback does. Each shown group carries `reporter: "server" | "client"`. Of the
  10 shown, the 5 largest server groups are kept first (`SERVER_ERROR_SLOTS = 5`), then the
  remaining slots go to the largest of everything else, server or client, and the shown list is
  ordered as before. So client reports can take at most 5 slots while server groups exist, server
  groups can take all 10 by count, and real client-side errors (browser apps report only through
  the client key) still show when the server has few. Ranking server groups strictly first was
  rejected: an app with 10 server error types would never show a client error.
- **The fetch is bounded per reporter.** The raw-group fetch keeps `FETCHED_GROUPS = 100` per
  reporter (`row_number() OVER (PARTITION BY reporter ...)`), so client spellings cannot fill the
  fetch and keep server groups out of it. At most 200 rows and 200 index seeks per call.
- **Merging after cleaning stays in Rust** (the #65 decision), keyed by
  `(cleaned type, flag_enabled, reporter)`; a server group and a client group of the same cleaned
  type stay separate rows.
- `ErrorGroup` loses its `sqlx::FromRow` derive (it gains a `KeyKind` field, which has no sqlx
  decoder); the query reads a tuple with a `server: bool` column and maps it. It is used nowhere
  else.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

`flag_health` reads each error group's newest message without aggregating the group's messages,
always shows a flag's declared variations, and always shows the server key's errors.

- The error query has no `array_agg`; each fetched group's message comes from one index seek on
  `(flag_id, occurred_at)`.
- Within a group, `last_message` is the message of the row with the latest `occurred_at`, ties
  to the highest `id`, honouring the environment filter.
- With 20 undeclared names each evaluated more than a flag's two declared variations, both declared
  variations are in `variations` with their exact counts (and a never-evaluated declared variation
  shows `0`).
- With server-key error groups present, client-key reports (any number of types or spellings)
  cannot reduce the server groups shown below `min(5, server groups)`.
- `evaluations`, `errors`, and `otherVariations` arithmetic stay exact.

## 5. Scope

**In:** `telemetry::health` (signature, three queries' shape, `top_variations`,
`merge_error_groups`), `ErrorGroup.reporter`, `Health` doc comments, the `flag_health` tool
description and `HEALTH_PROVENANCE` text, tests.

**Out:** ingest (`record_evaluations`, `record_errors`), the SDK REST contract, retention, the
auto-rollback checker, indexes and migrations, splitting `variations` by reporter (not asked;
declared names are the protection there).

## 6. Architecture

`crates/flags-core/src/telemetry.rs`:

- `pub async fn health(tx, flag: &FeatureFlag, environment, window_hours)` (was `flag: FlagId`).
  Declared names: `flag.variations.as_object()` keys (empty when not an object).
- Variations query (`health`, now line ~366):
  `SELECT variation, sum(count)::bigint, (sum(sum(count)) OVER ())::bigint FROM flag_eval_hourly
  WHERE <filter> AND enabled AND variation <> '' GROUP BY variation
  ORDER BY (variation = ANY($6)) DESC, 2 DESC, variation LIMIT $5`, `$6` the declared names
  (`text[]`).
- `top_variations(rows, declared: &[String], named_total)`: seed the map with every declared name
  at 0; add each row under its cleaned name (dropping empties); keep all declared entries plus the
  `SHOWN_VARIATIONS - declared.len()` largest undeclared (count desc, name asc); remainder
  `named_total - shown`, clamped at 0.
- Error query:

  ```sql
  WITH g AS (
    SELECT error_type, flag_enabled, reporter IS NOT DISTINCT FROM 'server' AS server,
           count(*)::bigint AS count, max(occurred_at) AS last_seen
    FROM flag_errors
    WHERE org_id = $1 AND flag_id = $2 AND occurred_at >= $3
      AND ($4::text IS NULL OR environment = $4 OR environment IS NULL)
    GROUP BY 1, 2, 3
  ), r AS (
    SELECT *, row_number() OVER (PARTITION BY server
                                 ORDER BY count DESC, error_type, flag_enabled) AS rn
    FROM g
  )
  SELECT r.error_type, r.flag_enabled, r.server, r.count, m.error_message, r.last_seen
  FROM r CROSS JOIN LATERAL (
    SELECT e.error_message FROM flag_errors e
    WHERE e.org_id = $1 AND e.flag_id = $2 AND e.occurred_at = r.last_seen
      AND e.error_type = r.error_type AND e.flag_enabled = r.flag_enabled
      AND (e.reporter IS NOT DISTINCT FROM 'server') = r.server
      AND ($4::text IS NULL OR e.environment = $4 OR e.environment IS NULL)
    ORDER BY e.id DESC LIMIT 1
  ) m
  WHERE r.rn <= $5
  ORDER BY r.server DESC, r.rn
  ```

  Every row the lateral seeks exists (the group's `max(occurred_at)` came from a matching row in
  the same snapshot), so `CROSS JOIN` drops nothing.
- `ErrorGroup` gains `pub reporter: KeyKind` (serialized `"server"` / `"client"`), after
  `flag_enabled`; drops `FromRow`.
- `merge_error_groups`: key `(type, enabled, reporter)`; rank as today (count desc, `last_seen`
  desc, type, on before off) plus server before client as a final tie-break; take the first
  `SERVER_ERROR_SLOTS` server groups in rank order, then fill to `SHOWN_ERRORS` from the rest in
  rank order; return in rank order.
- New const `SERVER_ERROR_SLOTS: usize = 5`, with its reasoning in its doc comment.

`crates/flags-mcp/src/tools/insight.rs`: pass `&flag`; the description says declared variations
are always listed and each error group names the kind of key that reported it.
`crates/flags-mcp/src/tools/out.rs` `HEALTH_PROVENANCE`: adds that a group marked `server` came
through the app's secret server key.

**Public interface (MCP `flag_health` output), additive:** `topErrors[].reporter` is new; a type
reported by both kinds of key shows as two groups; `variations` now also lists declared
variations with no evaluations (as `0`). `health`'s Rust signature changes (crate-internal
callers only: the tool and tests). The SDK REST contract is untouched.

## 7. Invariants impact

- **The database is small and shared:** strengthened. No per-group message array; the extra work
  is at most 200 index equality seeks, inside one statement (no per-row round trips).
- **SDK-reported text is untrusted:** unchanged; all text still passes `inert`. The new `reporter`
  value comes from the stored key kind, not from reported text.
- **Client keys are public:** strengthened; a client key can no longer hide declared variations or
  server errors from the agent.
- **No SQL outside flags-core; runtime queries only:** held.
- **Tenant isolation:** every new `flag_errors` reference (CTE and lateral) carries
  `org_id = $1` under RLS as before.
- **Reads not metered, SDK REST contract stable, no migration:** held.

## 8. Migration

None. `flag_errors_flag_time_idx (flag_id, occurred_at DESC)` serves the lateral seek.

## 9. Out-of-band

None: server code only, deployed by the merge. No new config, secret, or scope; no recurring cost.

## 10. Error handling and edge cases

- Flag with no declared variations (`{}`): behaves as today.
- `flag.variations` not an object (never stored so; `validate_variations` canonicalises): treated
  as no declared names.
- Two rows of a group at the same `occurred_at`: the higher `id` wins.
- Environment filter: the lateral applies the same filter, so a newer message in another
  environment is never chosen.
- Legacy rows with `reporter` NULL: client.
- Fewer than 5 server groups: the rest of the 10 go by count to anyone.
- No errors at all: empty list, as today.

## 11. Testing approach

Unit (beside the code, `telemetry.rs`):
- `top_variations`: declared names always kept (also at 0) while larger undeclared names fill only
  the remaining slots; a reported spelling that cleans to a declared name counts toward it; the
  existing tests pass with an empty declared list.
- `merge_error_groups`: server and client groups of one type stay apart; with 12 large client
  groups and 7 small server groups, the 5 largest server groups and 5 largest client groups are
  shown in rank order; with 2 server groups, 8 client groups fill the rest; the existing tests pass
  (helper gains a reporter).

DB (`crates/flags-core/tests/db.rs`, `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`):
- `health_shows_the_latest_message_of_each_error_group` (#69): one group with messages at
  increasing `occurred_at` inserted out of order, a tie at the latest instant (higher id wins),
  and a newer row in another environment that must not be chosen; a second group checks
  per-group lookup.
- `health_always_shows_declared_variations` (#70): flag declares `control`, `treatment`, `unused`;
  client key reports 25 undeclared names, each evaluated more than either; `variations` holds the
  three declared (with exact counts, `unused` at 0) and the 17 largest undeclared;
  `otherVariations` the rest.
- `health_keeps_server_errors_among_client_floods` (#70): 3 server error types (1 report each);
  over 100 client spellings plus 20 client types, each reported many times; the 3 server groups
  are shown, marked `server`.
- Existing health tests updated for the new signature (`&flag`); the e2e `flag_health` call
  (`crates/flags-server/tests/e2e.rs`) now expects `{"control": 0, "treatment": 0}` and
  `topErrors[0].reporter == "client"`.

Gates: test (`DATABASE_URL=… cargo test --workspace`), lint, format, extra (no migration), and the
`crates/**` podman build.

## 12. Risks and open questions

- A shared `occurred_at` across a very large number of rows (a client can send many events with
  one timestamp) makes the lateral seek walk those rows. That costs no more than the group
  aggregate already scanning them, and holds no memory beyond one row.
- Splitting by reporter shows a type reported by both keys as two rows, which uses two of the ten
  slots; accepted for the provenance it gives agents.
