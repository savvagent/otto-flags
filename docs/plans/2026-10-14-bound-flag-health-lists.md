# Bound flag_health Lists Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `flag_health` shows at most 20 variations (the rest summed in `otherVariations`) and one
`top_errors` row per error type as it reads after cleaning, with every SQL fetch behind the two
lists bounded.

**Architecture:** `telemetry::health` reads totals from one aggregate, fetches at most
`FETCHED_GROUPS` raw rows per list, and hands them to two pure functions (`top_variations`,
`merge_error_groups`) that clean, merge, rank, and cap. `Health` gains `other_variations`.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8 (runtime queries), Postgres 16.

**Ticket:** savvagent/otto-flags#59

**Spec:** `docs/specs/2026-10-14-bound-flag-health-lists-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-59-bound-flag-health-lists`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root. Every DB test run is serialized with
  sibling worktrees by the lock prefix
  `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock`:
  - test: `flock <lock> env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434.
- Invariants that apply: SDK-reported text is untrusted when an MCP tool returns it (both lists stay
  cleaned by `inert`); the database is small and shared (every fetch has a `LIMIT`); no SQL outside
  `flags-core`, runtime `sqlx::query`/`query_as` only; reads are not metered; no migration.
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #59`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/telemetry.rs` | constants, bounded queries, `top_variations`, `merge_error_groups`, `Health.other_variations`, unit tests |
| **Modify.** `crates/flags-core/src/eval.rs` | `MAX_VARIATIONS` becomes `pub(crate)` |
| **Modify.** `crates/flags-core/tests/db.rs` | DB tests for both lists |
| **Modify.** `crates/flags-mcp/src/tools/insight.rs` | `flag_health` description |

## Task Order & Rationale

Task 1 bounds variations (adds the field the MCP output carries); Task 2 merges error groups. They
touch disjoint parts of `health`, so either order works; variations first because it changes the
public shape.

---

### Task 1: Bound the variations flag_health shows

**Model:** sonnet

**Files:** `crates/flags-core/src/telemetry.rs`, `crates/flags-core/src/eval.rs`,
`crates/flags-core/tests/db.rs`, `crates/flags-mcp/src/tools/insight.rs`

- [ ] **Step 1: Write the failing tests.**
  - `telemetry.rs` `#[cfg(test)] mod tests` (create one at the end of the file if absent; reuse it
    if present): unit tests for `top_variations(rows: Vec<(String, i64)>, named_total: i64) ->
    (BTreeMap<String, i64>, i64)`:
    - 25 distinct names with counts 25..=1 and `named_total` = their sum: 20 names shown (counts
      25..=6), remainder = 5+4+3+2+1 = 15.
    - Ties at the cut: 21 names all count 1, `named_total = 21` → the 20 alphabetically first are
      kept, remainder 1.
    - Merge before cap: `"a\u{200B}b"` (1) and `"a\u{202E}b"` (1) become `"a b"` → 2.
    - A name that cleans to nothing (`"\u{200B}"`) is not shown and counts in the remainder.
    - `named_total` larger than the fetched rows (rows not fetched): the difference is in the
      remainder.
  - `crates/flags-core/tests/db.rs`, after `health_shows_variation_names_cleaned`, a
    `#[sqlx::test(migrator = "flags_core::MIGRATOR")]` test `health_shows_at_most_twenty_variations`
    (doc comment: a public client key can report any number of distinct names; the answer shows
    the 20 most evaluated, as many as a flag can declare, and sums the rest (#59)). Set up like
    `health_shows_variation_names_cleaned`; record, with `KeyKind::Client`, on-evaluations for 30
    names `v00`..`v29` where `vNN` gets `NN + 1` evaluations, plus 2 on-evaluations with
    `variation: None` and 3 off-evaluations. Assert `h.evaluations.enabled == 467` (sum 1..=30 = 465,
    plus 2), `h.evaluations.disabled == 3`, `h.variations.len() == 20`, `h.variations` keys are
    `v10`..`v29` with counts 11..=30, and `h.other_variations == 55` (sum 1..=10).
  - In `health_shows_variation_names_cleaned`, add `assert_eq!(h.other_variations, 1);` (the
    `"\u{200B}"` name) and extend its comment.
- [ ] **Step 2: Run, see them fail** (compile errors count as failing):
  `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core health_`
- [ ] **Step 3: Implement** in `telemetry.rs` per spec §6:
  - `eval.rs`: `const MAX_VARIATIONS` → `pub(crate) const MAX_VARIATIONS`.
  - Constants with doc comments: `SHOWN_VARIATIONS: usize = crate::eval::MAX_VARIATIONS` (as many
    as a flag can declare); `FETCHED_GROUPS: i64 = 100` (raw groups read per list before cleaning
    and merging; wider than what is shown because raw spellings that clean alike merge).
  - Replace the evaluations query and loop with: a totals query
    (`coalesce(sum(count) FILTER (WHERE enabled), 0)::bigint`, the same for `NOT enabled`, and for
    `enabled AND variation <> ''`), same `WHERE` as today; and a variations query
    `SELECT variation, sum(count)::bigint FROM flag_eval_hourly WHERE <same> AND enabled AND variation <> '' GROUP BY variation ORDER BY 2 DESC, variation LIMIT $5`
    binding `FETCHED_GROUPS`.
  - `fn top_variations` as in Step 1: clean with `inert(_, MAX_VARIATION)`, drop empty, merge,
    sort by count desc then name asc, keep `SHOWN_VARIATIONS`, remainder
    `(named_total - shown_sum).max(0)`.
  - `Health`: `pub other_variations: i64` right after `variations`, doc: on-evaluations that
    reported a variation name not in `variations` (beyond the 20 most evaluated, or one that
    cleans to nothing); evaluations with no variation are in neither. Update the `variations` doc:
    the 20 most evaluated names (as many as a flag may declare).
  - `insight.rs` `flag_health` description: after "the error rate on versus off," say "the most
    evaluated variations (the rest summed),".
- [ ] **Step 4: Run, see them pass:** the Step 2 command, then
  `cargo test -p flags-core --lib telemetry`, then all gates (Global Constraints), recording pass
  counts per test binary. `telemetry_rolls_up_and_feeds_health` and the e2e `flag_health` call must
  still pass.
- [ ] **Step 5: Commit:** `fix(core): show at most twenty variations in flag_health`, body: a
  public client key could size the answer and the query with distinct names; the remainder is a
  separate field because any key in the map is also a reportable name; ends `Fixes #59`. No
  attribution.

---

### Task 2: Merge error groups that read alike

**Model:** sonnet

**Files:** `crates/flags-core/src/telemetry.rs`, `crates/flags-core/tests/db.rs`

- [ ] **Step 1: Write the failing tests.**
  - Unit tests in `telemetry.rs` `mod tests` for `merge_error_groups(groups: Vec<ErrorGroup>) ->
    Vec<ErrorGroup>`:
    - Spellings that clean identically (`"TypeError"`, `"TypeError\u{200B}"` (hidden char →
      space → trimmed), `" TypeError"`), all `flag_enabled: true`, with different counts,
      `last_seen`, and `last_message`: one `"TypeError"` row, count = sum, `last_seen` = the
      latest, `last_message` from that latest group.
    - Two spellings of one type, one with `flag_enabled` true and one false, stay two rows.
    - 12 distinct types → 10 rows, highest counts kept; order count desc, then `last_seen` desc,
      then type.
    - `last_message` is shown cleaned and capped at `SHOWN_MESSAGE` (+ ellipsis), `error_type` at
      `MAX_ERROR_TYPE`.
  - `crates/flags-core/tests/db.rs` test `health_merges_error_types_that_read_alike`, set up like
    `health_shows_variation_names_cleaned` and calling
    `telemetry::health(&mut tx, flag.id, Some("production"), 24)` (doc: error
    types are grouped as they read after cleaning, so one error spread over spellings that differ
    only in hidden characters is one row and cannot push real errors out of the ten (#59)). Record
    with `KeyKind::Client`, flag on, environment `production`: 3 spellings of `TypeError` that
    clean alike (`"TypeError"`, `"TypeError\u{200B}"`, `"\u{202E}TypeError"`) with 2 errors each,
    and 11 other types `E00`..`E10` with 3 errors each (unmerged, no `TypeError` spelling would
    reach the ten). Assert `h.top_errors.len() == 10`, `h.top_errors[0].error_type == "TypeError"`
    with `count == 6`, exactly one `top_errors` entry reads `"TypeError"`, and the other 9 entries
    are distinct `E..` types with `count == 3`. `h.errors.enabled == 39`.
- [ ] **Step 2: Run, see them fail** (compile errors count as failing):
  `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core health_merges`
- [ ] **Step 3: Implement:**
  - `SHOWN_ERRORS: usize = 10` constant.
  - Errors query: `ORDER BY count DESC, error_type, flag_enabled LIMIT $5` binding
    `FETCHED_GROUPS`.
  - `fn merge_error_groups` per spec: clean `error_type` (`MAX_ERROR_TYPE`) and `last_message`
    (`SHOWN_MESSAGE`); merge by `(error_type, flag_enabled)` summing `count`, keeping the
    `last_seen`/`last_message` of the latest group; sort count desc, `last_seen` desc, type asc;
    truncate to `SHOWN_ERRORS`. Replaces the `.map(...)` cleaning block in `health`.
  - `Health.top_errors` doc: replace "They are grouped as reported and cleaned after, so two
    groups can read alike." with: grouped by how they read after cleaning (merged from the
    `FETCHED_GROUPS` most frequent raw groups, so a count can undercount a type reported under
    more spellings than that; `errors` is exact).
- [ ] **Step 4: Run, see them pass:** the Step 2 command, `cargo test -p flags-core --lib
  telemetry`, then all gates, with pass counts per test binary.
- [ ] **Step 5: Commit:** `fix(core): merge flag_health error groups that read alike`, body: raw
  grouping let one error spread over hidden-character spellings fill the ten slots; merging after
  cleaning keeps one definition of cleaning (no SQL copy) and leaves stored errors raw; ends
  `Fixes #59`. No attribution.

---

### Task 3: Record as shipped (after the PR number is known)

**Model:** haiku

- [ ] Spec `**Status:**` → `IMPLEMENTED (#<PR>)`; tick this plan's boxes. Commit
  `docs: record bound flag_health lists as shipped`, body ending `Fixes #59`.
