# Bound and merge the lists flag_health shows

**Status:** DRAFT
**Issue:** #59

## 1. Brief

From #59 (found by the reviews of #58 (#47); the behaviour predates it):

> `flag_health` (`crates/flags-core/src/telemetry.rs`, `health`) returns two lists whose size and
> contents a holder of an app's public `sdk_` client key controls:
>
> - **`variations`**: one entry per distinct reported variation name. Nothing checks the names
>   against the flag's declared variations, and the query has no `LIMIT`, so a client key can
>   create any number of distinct names (each up to 64 characters). Each becomes a
>   `flag_eval_hourly` row (bounded only by the 90-day rollup retention) and an entry in the
>   agent's answer.
> - **`top_errors`**: grouped in SQL by the raw `error_type` and cleaned by `inert` afterwards, so
>   error types that differ only in characters `inert` replaces show as separate, identical-looking
>   rows, and one error can be spread across several of the 10 slots, pushing real errors out.
>
> **Acceptance criteria**
> - [ ] The `variations` map `flag_health` returns is bounded (for example the top N by count, with
>   the rest summed under one "other" entry), or ingest records only variation names the flag
>   declares and counts any other as unknown.
> - [ ] `top_errors` groups that read alike after cleaning are shown as one group (or the grouping
>   key is the cleaned type).
> - [ ] Tests cover both.

## 2. Premise corrections

- **The evaluations query is unbounded too, not only the map.** `health` fetches one row per
  distinct `(enabled, variation)` in the window into memory before building the map, so bounding
  the map alone still lets a client key size a query result and the work `health` does per call.
  The fix bounds the fetch as well (Architecture).
- **The "other" entry cannot be a key in the map.** Any string chosen for it (`"other"`,
  `"(other)"`) is also a name a client key can report, so a reported name and the bucket would
  merge or collide. The remainder becomes its own field, `otherVariations`.

## 3. Assumptions

- **Bound the answer, not ingest** (the AC's first option). Recording only declared variation names
  would change what the SDK telemetry endpoint accepts (the SDK contract is additive-only), would
  need the flag's variations at ingest, and would misattribute evaluations from an SDK running
  against a flag whose variations were just renamed. The storage side is already bounded: the
  rollup row count is bounded by retention (90 days) and by the per-request batch size, as it was.
- **20 variations shown**, the most a flag may declare (`eval::MAX_VARIATIONS`), so a flag's own
  variations all fit unless reported junk outnumbers them. Chosen by count, ties by name.
- **100 raw groups fetched** for each list (`FETCHED_GROUPS`) before cleaning and merging. Raw names
  that clean alike are merged in Rust, so the fetch is wider than what is shown; 100 is cheap on the
  small database and leaves room for 5 raw spellings per shown variation.
- **Merge errors after cleaning, in Rust**, keyed by `(cleaned error_type, flag_enabled)`. Cleaning
  in SQL would need a second definition of `inert`'s character set (Unicode ranges, Rust's
  whitespace rule) that could drift from the first. Storing cleaned types at ingest would break the
  "error reports raw and truncated" invariant.
- A merged error group: `count` is the sum, `last_seen` the latest, `last_message` the message of
  the raw group with the latest `last_seen`. Order: count desc, then `last_seen` desc, then type, so
  the answer is deterministic. 10 shown, as before.
- `otherVariations` counts on-evaluations that reported a non-empty variation name not shown in
  `variations`: names beyond the top 20, names outside the fetched 100, and names that clean to
  nothing (already dropped from the map today). Evaluations with no variation are in neither, as
  today. Computed as `named total - sum(shown)` from a totals query. The totals and the variations
  query are two statements under READ COMMITTED, so an ingest landing between them can skew the
  remainder by that batch; `max(0)` keeps it non-negative. Accepted: health is a reading of a live
  window, not a ledger.
- Ties: the SQL `ORDER BY ..., variation` (database collation) only makes the fetch deterministic;
  what is shown is decided by the Rust sort (count desc, then byte order of the cleaned name).
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

What a client key reports can no longer size `flag_health`'s answer or split one error into
several rows.

- 50 distinct reported variation names produce at most 20 `variations` entries (the 20 most
  evaluated), with every other named on-evaluation counted in `otherVariations`.
- Error types differing only in characters `inert` replaces show as one `top_errors` row whose
  count is their sum.
- `evaluations` and `errors` totals are unchanged by either (they come from their own aggregates).
- The SQL fetch behind each list has a `LIMIT`.

## 5. Scope

**In:** `telemetry::health`, the `Health` doc comments, a new `Health.other_variations` field, the
`flag_health` tool description, tests.

**Out:** ingest (`record_evaluations`, `record_errors`), rollup retention, the auto-rollback checker
(it reads totals only), the SDK REST contract.

## 6. Architecture

`crates/flags-core/src/telemetry.rs`:

- Constants: `SHOWN_VARIATIONS = crate::eval::MAX_VARIATIONS` (made `pub(crate)` in `eval.rs`),
  `SHOWN_ERRORS = 10`, `FETCHED_GROUPS: i64 = 100`.
- Totals query replaces the per-variation loop's counting:
  `SELECT coalesce(sum(count) FILTER (WHERE enabled), 0)::bigint, coalesce(sum(count) FILTER (WHERE NOT enabled), 0)::bigint, coalesce(sum(count) FILTER (WHERE enabled AND variation <> ''), 0)::bigint FROM flag_eval_hourly WHERE <same window/env filter>`.
- Variations query: `SELECT variation, sum(count)::bigint FROM flag_eval_hourly WHERE <filter> AND enabled AND variation <> '' GROUP BY variation ORDER BY 2 DESC, variation LIMIT $5`.
- Pure fn `top_variations(rows: Vec<(String, i64)>, named_total: i64) -> (BTreeMap<String, i64>, i64)`:
  clean each name with `inert(_, MAX_VARIATION)`, drop empties, merge alike, keep the
  `SHOWN_VARIATIONS` largest (count desc, name asc), return them and
  `named_total - sum(shown)` (never negative: `max(0)`).
- Errors query: as today, but `ORDER BY count DESC, error_type, flag_enabled LIMIT $5` with
  `FETCHED_GROUPS`.
- Pure fn `merge_error_groups(groups: Vec<ErrorGroup>) -> Vec<ErrorGroup>`: clean `error_type`
  (`MAX_ERROR_TYPE`) and `last_message` (`SHOWN_MESSAGE`), merge by `(error_type, flag_enabled)`
  per the Assumptions, sort, truncate to `SHOWN_ERRORS`.
- `Health` gains `pub other_variations: i64` (serialized `otherVariations`), after `variations`.
  Its doc and the `variations` / `top_errors` docs say what is bounded and merged.

`crates/flags-mcp/src/tools/insight.rs`: the `flag_health` description mentions the most evaluated
variations (others summed). `HealthOut` embeds `Health`, so the field reaches the tool's output
schema with no other change.

Public interface: one additive field on the `flag_health` output. `variations` may now hold fewer
entries than before (a cap that nothing legitimate reaches past 20 declared names).

## 7. Invariants impact

- **SDK-reported text is untrusted:** unchanged; both lists are still cleaned by `inert`, and now
  bounded.
- **The database is small and shared:** strengthened; the per-call fetch is bounded.
- **No SQL outside flags-core / runtime queries only:** held.
- **Reads are not metered:** unchanged.
- **No migration.**

## 8. Migration

None.

## 9. Out-of-band

None. The change is in server code and deploys with the merge. No recurring-cost impact.

## 10. Error handling and edge cases

- No evaluations: totals are 0 (`coalesce`), `variations` empty, `otherVariations` 0.
- Ties at the 20th variation: name order decides, so the answer is stable.
- A merged variation can rank in the top 20 only through spellings within the fetched 100; any
  spelling outside them is in `otherVariations`, so the totals still add up.
- A merged error group's count sums only the raw spellings among the 100 most frequent raw groups;
  `errors` (the totals) stays exact. Spreading one error over more than 100 raw spellings only
  lowers its own row.
- Names that clean to the empty string are dropped from the map and counted in `otherVariations`.

## 11. Testing approach

- Unit tests beside the code in `telemetry.rs` for `top_variations` (cap, ordering and ties,
  merge-before-cap, empties, remainder) and `merge_error_groups` (merge sums counts, keeps the
  latest message and `last_seen`, keeps on/off apart, caps at 10, order).
- `crates/flags-core/tests/db.rs`:
  - `health_shows_at_most_twenty_variations`: 30 names with distinct counts (plus one
    no-variation evaluation); `variations` holds the 20 most evaluated, `otherVariations` the rest,
    `evaluations.enabled` the full total.
  - `health_merges_error_types_that_read_alike`: three spellings of `TypeError` that clean to the
    same text, 2 errors each, against eleven other types of 3 errors each. Unmerged, no spelling
    reaches the ten; merged, `TypeError` (6) ranks first as one row and nine of the eleven follow.
  - `health_shows_variation_names_cleaned` gains `otherVariations == 1` (the `"\u{200B}"` name).
- Gates: test (under the shared DB-test lock), lint, format, extra, and the `crates/**` podman
  build.

## 12. Risks and open questions

- Undercounting a merged error group past 100 raw spellings (Error handling) is accepted: exact
  merging needs the cleaned key in SQL or at ingest, both rejected in Assumptions.
