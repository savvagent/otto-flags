# flag_health Crowding and Message Lookup Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `flag_health` reads each error group's newest message by primary key instead of from an
array of every message (#69), always lists a flag's declared variations, and keeps server-key
error groups among those shown (#70).

**Architecture:** `telemetry::health` takes the `&FeatureFlag`. The error query aggregates counts,
`max(occurred_at)`, and the newest row's id per group in one pass, keeps the 100 largest groups
per reporter, and reads each kept group's message by primary key. The variations
query orders declared names first before its `LIMIT`. Two pure functions (`top_variations`,
`merge_error_groups`) apply the declared-names and server-slots rules.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8 (runtime queries), Postgres 16.

**Ticket:** savvagent/otto-flags#69, savvagent/otto-flags#70

**Spec:** `docs/specs/2026-10-14-flag-health-crowding-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-69-flag-health-bounded-queries`);
  `main` changes only through the PR. Never commit or push to `main`.
- Gates (from `CLAUDE.md` > Gates), run from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
    (one test: append `-p flags-core --test db <name>` or `-p flags-core --lib <name>`; the
    `DATABASE_URL` must be set either way)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .` (final run only)
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434. If a DB run fails
  with connection errors, check `podman ps` and re-run before treating it as a failure.
- Invariants that apply: the database is small and shared (no per-message aggregation, every fetch
  bounded, no per-row round trips); SDK-reported text is untrusted when an MCP tool returns it (all
  text still passes `inert`); tenant isolation (every `flag_errors` reference filters
  `org_id = $1`); no SQL outside `flags-core`, runtime `sqlx::query`/`query_as` only; the SDK
  REST contract is unchanged; reads are not metered; no migration.
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`; body explains why and ends with
  `Fixes #69` and/or `Fixes #70` as the task says. No attribution of any kind (no
  Co-Authored-By, no "Generated with", no credit markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/telemetry.rs` | `health` signature and queries, `top_variations`, `merge_error_groups`, `ErrorGroup.reporter`, `SERVER_ERROR_SLOTS`, docs, unit tests |
| **Modify.** `crates/flags-core/tests/db.rs` | DB tests; existing `health` calls pass `&flag` |
| **Modify.** `crates/flags-mcp/src/tools/insight.rs` | pass `&flag`; tool description |
| **Modify.** `crates/flags-mcp/src/tools/out.rs` | `HEALTH_PROVENANCE` text |
| **Modify.** `crates/flags-server/tests/e2e.rs` | `flag_health` output assertions |

## Task Order & Rationale

Task 1 (#69) rewrites the error query without changing output, so its test pins the old
behaviour before the reporter split. Task 2 (#70, variations) changes `health`'s signature, which
Task 3 then relies on. Task 3 (#70, errors) extends Task 1's query.

---

### Task 1: Look up each error group's newest message by primary key (#69)

**Files:**
- Modify: `crates/flags-core/src/telemetry.rs` (`health`, the `top_errors` query)
- Test: `crates/flags-core/tests/db.rs`

**Interfaces:** produces the `WITH g AS (...) ... JOIN flag_errors e ON e.id = g.last_id` query shape Task 3 extends.

**Out-of-band / invariants:** none out-of-band; database small and shared; tenant isolation.

**Model:** sonnet — a contained query rewrite the spec writes out.

- [ ] **Step 1: Write the failing test** in `crates/flags-core/tests/db.rs`, after
  `health_merges_error_types_that_read_alike`. It inserts rows directly (to control `id` order and
  timestamps) through the tenant transaction, which RLS allows for the pinned org:

```rust
// Each error group shows the message of its latest report: the newest
// occurred_at, the row stored last on a tie, within the environment asked
// about. It is looked up per shown group, not aggregated from every message
// (#69).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_shows_the_latest_message_of_each_error_group(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
    let flag = tx
        .create_flag(
            &app,
            NewFlag {
                key: "f".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();

    let now = Utc::now().duration_trunc(TimeDelta::seconds(1)).unwrap();
    let ago = |mins: i64| now - TimeDelta::minutes(mins);
    // (environment, error_type, message, occurred_at), in insertion (id) order.
    let rows = [
        (Some("production"), "TypeError", "newest", ago(1)),
        (Some("production"), "TypeError", "oldest", ago(30)),
        (Some("production"), "TypeError", "tied, stored first", ago(1)),
        (Some("production"), "TypeError", "tied, stored last", ago(1)),
        (Some("production"), "TypeError", "middle", ago(10)),
        // Newer, but in another environment.
        (Some("staging"), "TypeError", "other environment", ago(0)),
        (None, "RangeError", "no environment, newest", ago(2) + TimeDelta::microseconds(1)),
        // A microsecond older but stored later: the time decides, not the id.
        (Some("production"), "RangeError", "a microsecond older", ago(2)),
    ];
    for (environment, error_type, message, at) in rows {
        sqlx::query(
            "INSERT INTO flag_errors \
               (org_id, flag_id, environment, flag_enabled, error_type, error_message, occurred_at, reporter) \
             VALUES ($1, $2, $3, true, $4, $5, $6, 'server')",
        )
        .bind(org)
        .bind(flag.id)
        .bind(environment)
        .bind(error_type)
        .bind(message)
        .bind(at)
        .execute(tx.conn())
        .await
        .unwrap();
    }

    let h = telemetry::health(&mut tx, flag.id, Some("production"), 24)
        .await
        .unwrap();
    let shown: Vec<(&str, i64, &str)> = h
        .top_errors
        .iter()
        .map(|g| (g.error_type.as_str(), g.count, g.last_message.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            ("TypeError", 5, "tied, stored last"),
            ("RangeError", 2, "no environment, newest"),
        ]
    );
    assert_eq!(h.top_errors[0].last_seen, ago(1));
    assert_eq!(
        h.top_errors[1].last_seen,
        ago(2) + TimeDelta::microseconds(1)
    );

    // Across every environment, the staging report is the newest.
    let h = telemetry::health(&mut tx, flag.id, None, 24).await.unwrap();
    assert_eq!(h.top_errors[0].count, 6);
    assert_eq!(h.top_errors[0].last_message, "other environment");
    tx.commit().await.unwrap();
}
```

  Add `DurationRound` to the file's `chrono` import if it is not there (`use chrono::{DurationRound, TimeDelta, Utc};` alongside what is imported). Truncating `now` to whole seconds keeps the Postgres round trip exact (microsecond precision) for the `last_seen` comparison.

- [ ] **Step 2: Run it and see it fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db health_shows_the_latest_message_of_each_error_group`
Expected: FAIL on the first `assert_eq!` — the tie at `ago(1)` resolves arbitrarily under
`array_agg` (if it happens to pass, the test is still the pin for the new query; note it in the
report and continue).

- [ ] **Step 3: Implement.** In `health`, replace the `top_errors` query string with:

```rust
    // Counts, the latest time, and the newest row's id per group in one
    // pass; the message only for the groups kept, by primary key (#69).
    // Aggregating every message to keep one grew with whatever a public
    // client key reported, and a per-group seek through
    // flag_errors_flag_time_idx (which has no error_type) would read every row
    // at that instant once per group. `max` over [epoch, id] compares element
    // by element: the latest occurred_at, then the highest id (the row stored
    // last) on a tie, in fixed space per group.
    let top_errors: Vec<ErrorGroup> = sqlx::query_as(
        "WITH g AS ( \
           SELECT error_type, flag_enabled, count(*)::bigint AS count, \
                  max(occurred_at) AS last_seen, \
                  (max(ARRAY[extract(epoch FROM occurred_at), id::numeric]))[2]::bigint AS last_id \
           FROM flag_errors \
           WHERE org_id = $1 AND flag_id = $2 AND occurred_at >= $3 \
             AND ($4::text IS NULL OR environment = $4 OR environment IS NULL) \
           GROUP BY error_type, flag_enabled \
           ORDER BY count DESC, error_type, flag_enabled LIMIT $5 \
         ) \
         SELECT g.error_type, g.flag_enabled, g.count, e.error_message AS last_message, g.last_seen \
         FROM g JOIN flag_errors e ON e.org_id = $1 AND e.id = g.last_id \
         ORDER BY g.count DESC, g.error_type, g.flag_enabled",
    )
```

  keeping the same `.bind(org) .bind(flag) .bind(since) .bind(environment.as_deref()) .bind(FETCHED_GROUPS)`.

- [ ] **Step 4: Run it and see it pass**, then the whole `health` family:

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db health`
then `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db telemetry`
Expected: PASS, none skipped.

- [ ] **Step 5: Commit**

```bash
git add crates/flags-core/src/telemetry.rs crates/flags-core/tests/db.rs
git commit -m "fix(core): look up each error group's newest message instead of aggregating them

flag_health built an array of every message in each error group to keep the
newest one, before the LIMIT on groups. A public client key decides how many
messages a group holds, so the array could press on the shared instance's
memory. The aggregate now carries only the newest row's id per group (the
last stored on a tie), and each kept group's message is read by primary key.

Fixes #69"
```

---

### Task 2: Always list a flag's declared variations (#70)

**Files:**
- Modify: `crates/flags-core/src/telemetry.rs` (`health` signature, variations query,
  `top_variations`, `Health.variations` doc, unit tests)
- Modify: `crates/flags-core/tests/db.rs` (every `telemetry::health(&mut tx, flag.id, ...)` call
  becomes `telemetry::health(&mut tx, &flag, ...)`, including Task 1's; new test)
- Modify: `crates/flags-mcp/src/tools/insight.rs` (pass `&flag`; description)
- Modify: `crates/flags-server/tests/e2e.rs` (variations expectation)

**Interfaces:** `pub async fn health(tx: &mut Tx<'_>, flag: &FeatureFlag, environment: Option<&str>, window_hours: i64) -> Result<Health>`, used by Task 3.

**Out-of-band / invariants:** none out-of-band; client keys are public; SDK-reported text
untrusted; database small and shared.

**Model:** sonnet — the spec fixes every rule.

- [ ] **Step 1: Write the failing tests.**

  Unit tests in `telemetry.rs` `mod tests` (update the existing `top_variations(rows, total)`
  calls to `top_variations(rows, &[], total)` and add):

```rust
    fn declared(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn declared_variations_are_always_shown() {
        // 25 undeclared names, each evaluated more than either declared one.
        let mut rows = names(25, |i| 100 - i as i64);
        rows.push(("treatment".into(), 3));
        let total: i64 = rows.iter().map(|(_, n)| n).sum();
        let (shown, other) = top_variations(rows, &declared(&["control", "treatment"]), total);
        assert_eq!(shown.len(), SHOWN_VARIATIONS);
        assert_eq!(shown.get("treatment"), Some(&3));
        assert_eq!(shown.get("control"), Some(&0));
        // Undeclared names fill the other 18 slots, largest first.
        assert!(shown.contains_key("v17"));
        assert!(!shown.contains_key("v18"));
        // v00..v24 have 100..=76 evaluations; v18..v24 (82..=76) are not shown.
        assert_eq!(other, (76..=82).sum::<i64>());
    }
```

```rust
    #[test]
    fn a_spelling_of_a_declared_variation_counts_toward_it() {
        let rows = vec![("on".to_string(), 2), ("on\u{200B}".to_string(), 3)];
        let (shown, other) = top_variations(rows, &declared(&["on"]), 5);
        assert_eq!(shown, BTreeMap::from([("on".to_string(), 5)]));
        assert_eq!(other, 0);
    }
```

  Before relying on it, check what `inert("on\u{200B}", MAX_VARIATION)` returns (see the existing
  `variations_merge_before_the_cap` test: hidden characters become a space and the result may be
  trimmed). If it is not exactly `"on"`, use a spelling that does clean to `"on"` (for example
  `"\u{200B}on"` if leading hidden characters are trimmed) and say which in the report.

  DB test in `crates/flags-core/tests/db.rs`, after `health_reads_a_bounded_number_of_variation_names`:

```rust
// A flag's declared variations are always shown, even when a public client
// key reports more evaluations under made-up names, and even at zero (#70).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_always_shows_declared_variations(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
    let flag = tx
        .create_flag(
            &app,
            NewFlag {
                key: "f".into(),
                variations: Some(json!({"control": {}, "treatment": {}, "unused": {}})),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();

    let now = Utc::now();
    let event = |variation: String| EvalEvent {
        flag_key: "f".into(),
        enabled: true,
        variation: Some(variation),
        environment: "production".into(),
        at: now,
    };
    // control 2, treatment 3; junk00..junk24 evaluated 10..=34 times.
    let mut events: Vec<EvalEvent> = (0..2).map(|_| event("control".into())).collect();
    events.extend((0..3).map(|_| event("treatment".into())));
    for i in 0..25 {
        events.extend((0..(10 + i)).map(|_| event(format!("junk{i:02}"))));
    }
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Client, events)
        .await
        .unwrap();

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!(h.variations.len(), 20);
    assert_eq!(h.variations.get("control"), Some(&2));
    assert_eq!(h.variations.get("treatment"), Some(&3));
    assert_eq!(h.variations.get("unused"), Some(&0));
    // The 17 most evaluated junk names (junk08..junk24) take the other slots.
    assert!(h.variations.contains_key("junk08"));
    assert!(!h.variations.contains_key("junk07"));
    // junk00..junk07: 10 + 11 + ... + 17.
    assert_eq!(h.other_variations, (10..=17).sum::<i64>());
    tx.commit().await.unwrap();
}
```

  Check `NewFlag`'s `variations` field type in `crates/flags-core/src/flags.rs` (around line 111:
  `pub variations: Option<Value>`) and that `json!` is imported in `db.rs`; adjust if not.

- [ ] **Step 2: Run and see them fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core`
Expected: compile errors (new signature) — that is the failing state.

- [ ] **Step 3: Implement.**
  - `health(tx, flag: &FeatureFlag, environment, window_hours)`: import
    `crate::flags::FeatureFlag`; use `flag.id` where `flag` was bound. Declared names:
    ```rust
    // Declared names pass validate_name, so inert leaves them as they are.
    let declared: Vec<String> = flag
        .variations
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    ```
  - Variations query: order declared names first, bind them as `$6`:
    ```sql
    SELECT variation, sum(count)::bigint, (sum(sum(count)) OVER ())::bigint
    FROM flag_eval_hourly
    WHERE org_id = $1 AND flag_id = $2 AND hour >= date_trunc('hour', $3::timestamptz)
      AND ($4::text IS NULL OR environment = $4) AND enabled AND variation <> ''
    GROUP BY variation ORDER BY variation = ANY($6) DESC, 2 DESC, variation LIMIT $5
    ```
    with `.bind(&declared)` last, and update the comment above it: declared names come first so
    they are always within the fetch.
  - `top_variations(rows: Vec<(String, i64)>, declared: &[String], named_total: i64)`:
    ```rust
    fn top_variations(
        rows: Vec<(String, i64)>,
        declared: &[String],
        named_total: i64,
    ) -> (BTreeMap<String, i64>, i64) {
        let mut merged: HashMap<String, i64> =
            declared.iter().map(|d| (inert(d, MAX_VARIATION), 0)).collect();
        for (name, n) in rows {
            let name = inert(&name, MAX_VARIATION);
            if !name.is_empty() {
                *merged.entry(name).or_insert(0) += n;
            }
        }
        let (mut shown, mut undeclared): (BTreeMap<String, i64>, Vec<(String, i64)>) =
            (BTreeMap::new(), vec![]);
        for (name, n) in merged {
            if declared.iter().any(|d| *d == name) {
                shown.insert(name, n);
            } else {
                undeclared.push((name, n));
            }
        }
        undeclared.sort_by(|(a, x), (b, y)| y.cmp(x).then_with(|| a.cmp(b)));
        undeclared.truncate(SHOWN_VARIATIONS.saturating_sub(shown.len()));
        shown.extend(undeclared);
        let total_shown: i64 = shown.values().sum();
        (shown, (named_total - total_shown).max(0))
    }
    ```
    Update its doc comment: declared names always kept (at 0 if not evaluated), undeclared fill
    the remaining slots.
  - `Health.variations` doc: the flag's declared variations always (at 0 when none were
    evaluated), then the most evaluated other names up to `SHOWN_VARIATIONS` in all.
  - `insight.rs`: `telemetry::health(&mut tx, &flag, ...)`; description (the phrase spans a `\`
    line continuation, so edit both lines): replace "the most
    evaluated variations (the rest summed in otherVariations)" with "evaluations per variation
    (every declared variation, then the most evaluated other names reported; the rest summed in
    otherVariations)".
  - `db.rs`: every existing `telemetry::health(&mut tx, flag.id,` becomes
    `telemetry::health(&mut tx, &flag,`.
  - `e2e.rs` (around line 622): the flag declares `control` and `treatment` and no evaluation
    reported a variation, so replace the `variations` assertion and its comment with:
    ```rust
    // No variations were reported: the declared ones at zero and nothing
    // summed, under the wire names agents read.
    assert_eq!(health["health"]["variations"], json!({"control": 0, "treatment": 0}));
    ```

- [ ] **Step 4: Run and see them pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
Expected: all pass, none skipped. Run `cargo fmt --all` first. Then `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --all -- --check` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/flags-core crates/flags-mcp crates/flags-server/tests/e2e.rs
git commit -m "fix(core): always list a flag's declared variations in flag_health

Variations were ranked purely by count, so anyone holding the public client
key could report made-up names often enough to push the flag's own
variations out of the answer. Declared names are now fetched first and always
shown (at zero when none were evaluated); other names fill the slots left.

Fixes #70"
```

---

### Task 3: Split error groups by reporter and keep slots for the server key (#70)

**Files:**
- Modify: `crates/flags-core/src/telemetry.rs` (error query, `ErrorGroup.reporter`,
  `merge_error_groups`, `SERVER_ERROR_SLOTS`, `Health.top_errors` doc, unit tests)
- Modify: `crates/flags-core/tests/db.rs` (new test)
- Modify: `crates/flags-mcp/src/tools/insight.rs` (description), `crates/flags-mcp/src/tools/out.rs`
  (`HEALTH_PROVENANCE`)
- Modify: `crates/flags-server/tests/e2e.rs` (`reporter` assertion)

**Interfaces:** consumes Task 1's query and Task 2's `health(&flag)` signature.

**Out-of-band / invariants:** none out-of-band; client keys are public; database small and shared
(at most 200 fetched groups and 200 primary-key lookups); tenant isolation.

**Model:** sonnet — the spec fixes the query and the ranking rule.

- [ ] **Step 1: Write the failing tests.**

  Unit tests in `telemetry.rs`: change the `group` helper to take a reporter and give every
  existing call `KeyKind::Server` (their expectations are unchanged):
  ```rust
  fn group(error_type: &str, on: bool, count: i64, secs: i64, msg: &str) -> ErrorGroup {
      reported(error_type, on, KeyKind::Server, count, secs, msg)
  }

  fn reported(error_type: &str, on: bool, reporter: KeyKind, count: i64, secs: i64, msg: &str) -> ErrorGroup {
      ErrorGroup {
          error_type: error_type.into(),
          flag_enabled: on,
          reporter,
          count,
          last_message: msg.into(),
          last_seen: DateTime::from_timestamp(secs, 0).unwrap(),
      }
  }
  ```
  New tests:
  ```rust
  #[test]
  fn error_groups_keep_reporters_apart() {
      let merged = merge_error_groups(vec![
          reported("TypeError", true, KeyKind::Server, 3, 100, "s"),
          reported("TypeError", true, KeyKind::Client, 2, 200, "c"),
      ]);
      assert_eq!(
          merged,
          vec![
              reported("TypeError", true, KeyKind::Server, 3, 100, "s"),
              reported("TypeError", true, KeyKind::Client, 2, 200, "c"),
          ]
      );
  }

  #[test]
  fn client_reports_cannot_crowd_out_server_errors() {
      // 12 large client groups and 7 small server ones: the 5 largest
      // server groups keep their slots, the 5 largest client groups the rest.
      let mut groups: Vec<ErrorGroup> = (0..12)
          .map(|i| reported(&format!("C{i:02}"), true, KeyKind::Client, 100 + i, 100, "m"))
          .collect();
      groups.extend((0..7).map(|i| reported(&format!("S{i:02}"), true, KeyKind::Server, 1 + i, 100, "m")));
      let shown: Vec<String> = merge_error_groups(groups).into_iter().map(|g| g.error_type).collect();
      assert_eq!(shown, ["C11", "C10", "C09", "C08", "C07", "S06", "S05", "S04", "S03", "S02"]);
  }

  #[test]
  fn server_slots_left_unused_go_by_count() {
      let mut groups: Vec<ErrorGroup> = (0..12)
          .map(|i| reported(&format!("C{i:02}"), true, KeyKind::Client, 100 + i, 100, "m"))
          .collect();
      groups.extend((0..2).map(|i| reported(&format!("S{i:02}"), true, KeyKind::Server, 1 + i, 100, "m")));
      let shown: Vec<String> = merge_error_groups(groups).into_iter().map(|g| g.error_type).collect();
      assert_eq!(shown, ["C11", "C10", "C09", "C08", "C07", "C06", "C05", "C04", "S01", "S00"]);
  }
  ```

  DB test in `crates/flags-core/tests/db.rs`, after Task 1's test:
  ```rust
  // A public client key can report any number of error types and spellings.
  // Server-key groups are fetched apart from client ones and keep slots in
  // the answer, so client reports cannot hide what the server reported (#70).
  #[sqlx::test(migrator = "flags_core::MIGRATOR")]
  async fn health_keeps_server_errors_among_client_floods(pool: PgPool) {
      let db = db(pool).await;
      let org = OrgId::new();
      let actor = UserId::new();
      let mut tx = db.begin(org).await.unwrap();
      let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
      let flag = tx
          .create_flag(
              &app,
              NewFlag {
                  key: "f".into(),
                  ..Default::default()
              },
              actor,
              None,
          )
          .await
          .unwrap();

      let now = Utc::now();
      let error = |error_type: String| ErrorEvent {
          flag_key: "f".into(),
          flag_enabled: true,
          environment: Some("production".into()),
          error_type,
          error_message: "boom".into(),
          stack_trace: None,
          at: now,
      };
      // 120 spellings of one junk type (3 each) fill a shared fetch of 100,
      // and 20 other client types (5 each) outrank every server type.
      let mut flood: Vec<ErrorEvent> = (1..=120)
          .flat_map(|k| {
              let t = format!("Junk{}", "\u{200B}".repeat(k));
              (0..3).map(move |_| t.clone())
          })
          .map(error)
          .collect();
      flood.extend((0..20).flat_map(|i| (0..5).map(move |_| format!("C{i:02}"))).map(error));
      for batch in flood.chunks(telemetry::MAX_BATCH) {
          telemetry::record_errors(&mut tx, app.id, KeyKind::Client, batch.to_vec())
              .await
              .unwrap();
      }
      let server = ["S0", "S1", "S2"].map(|t| error(t.into())).to_vec();
      telemetry::record_errors(&mut tx, app.id, KeyKind::Server, server)
          .await
          .unwrap();

      let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
          .await
          .unwrap();
      assert_eq!(h.top_errors.len(), 10);
      let server: Vec<&str> = h
          .top_errors
          .iter()
          .filter(|g| g.reporter == KeyKind::Server)
          .map(|g| g.error_type.as_str())
          .collect();
      assert_eq!(server, ["S0", "S1", "S2"]);
      assert!(h
          .top_errors
          .iter()
          .filter(|g| g.reporter == KeyKind::Client)
          .all(|g| g.count >= 5));
      tx.commit().await.unwrap();
  }
  ```

  In `crates/flags-server/tests/e2e.rs`, after the `otherVariations` assertion near line 625, add
  (the errors there were reported with the client key):
  ```rust
  assert_eq!(health["health"]["topErrors"][0]["reporter"], "client");
  ```

- [ ] **Step 2: Run and see them fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core`
Expected: compile errors (`ErrorGroup` has no `reporter`).

- [ ] **Step 3: Implement.**
  - `ErrorGroup`: drop `FromRow` from its derive list; add after `flag_enabled`:
    ```rust
    /// The kind of SDK key that reported this group. `server` reports came
    /// through the app's secret server key; `client` reports through the
    /// public client key, which anyone can use (reports stored before the
    /// reporter was recorded count as client).
    pub reporter: KeyKind,
    ```
    Remove `use sqlx::FromRow;` if nothing else uses it.
  - Constant after `SHOWN_ERRORS`:
    ```rust
    /// Of the `SHOWN_ERRORS`, how many are kept for the largest server-key
    /// groups when there are that many. A public client key can report any
    /// number of error types, so ranking by count alone let it push every
    /// server-key error out of the answer (#70). Half, not all: browser apps
    /// report real errors only through the client key, and those still show
    /// in the other slots.
    const SERVER_ERROR_SLOTS: usize = 5;
    ```
  - Error query (replacing Task 1's), fetched as tuples and mapped:
    ```rust
    let rows: Vec<(String, bool, bool, i64, String, DateTime<Utc>)> = sqlx::query_as(
        "WITH g AS ( \
           SELECT error_type, flag_enabled, reporter IS NOT DISTINCT FROM 'server' AS server, \
                  count(*)::bigint AS count, max(occurred_at) AS last_seen, \
                  (max(ARRAY[extract(epoch FROM occurred_at), id::numeric]))[2]::bigint AS last_id \
           FROM flag_errors \
           WHERE org_id = $1 AND flag_id = $2 AND occurred_at >= $3 \
             AND ($4::text IS NULL OR environment = $4 OR environment IS NULL) \
           GROUP BY 1, 2, 3 \
         ), r AS ( \
           SELECT *, row_number() OVER ( \
             PARTITION BY server ORDER BY count DESC, error_type, flag_enabled) AS rn \
           FROM g \
         ), kept AS ( \
           SELECT * FROM r WHERE rn <= $5 \
         ) \
         SELECT k.error_type, k.flag_enabled, k.server, k.count, e.error_message, k.last_seen \
         FROM kept k JOIN flag_errors e ON e.org_id = $1 AND e.id = k.last_id \
         ORDER BY k.server DESC, k.rn",
    )
    // same five binds
    ...;
    let top_errors = rows
        .into_iter()
        .map(|(error_type, flag_enabled, server, count, last_message, last_seen)| ErrorGroup {
            error_type,
            flag_enabled,
            reporter: if server { KeyKind::Server } else { KeyKind::Client },
            count,
            last_message,
            last_seen,
        })
        .collect();
    ```
    Update the comment above it: the fetch keeps `FETCHED_GROUPS` per reporter, so client groups
    cannot fill it.
  - `merge_error_groups`: remove the existing `ranked.truncate(SHOWN_ERRORS)` (it would cut
    server groups before the slots apply); key `(String, bool, KeyKind)` (add `Hash` to `KeyKind`'s derive in
    `crates/flags-core/src/keys.rs`); rank as today with
    `.then_with(|| (b.reporter == KeyKind::Server).cmp(&(a.reporter == KeyKind::Server)))` as the
    last tie-break; then:
    ```rust
    let mut shown = Vec::with_capacity(SHOWN_ERRORS);
    let mut rest = vec![];
    for g in ranked {
        let server_slots_left = shown.len() < SERVER_ERROR_SLOTS;
        if g.reporter == KeyKind::Server && server_slots_left {
            shown.push(g);
        } else {
            rest.push(g);
        }
    }
    ```
    Careful: `shown.len()` counts only server groups at this point (only server groups are pushed
    in the loop), which is what the slot limit means. Then
    `shown.extend(rest.into_iter().take(SHOWN_ERRORS - shown.len()))`, re-sort `shown` with the
    same comparator (extract it into a local closure or fn `rank(a, b) -> Ordering` used twice),
    and return it. Update the doc comment: groups are per reporter; the `SERVER_ERROR_SLOTS`
    largest server groups are kept first.
  - `Health.top_errors` doc: groups are per reporter; the fetch is bounded per reporter; the
    largest server groups keep `SERVER_ERROR_SLOTS` of the slots.
  - `insight.rs` description: "the most common errors (each naming the kind of SDK key that
    reported it)".
  - `out.rs` `HEALTH_PROVENANCE`: after the sentence about the public client key add: "Error
    groups marked reporter \"server\" came through the app's secret server key."

- [ ] **Step 4: Run and see them pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
Expected: all pass, none skipped. Run `cargo fmt --all` first. Then `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --all -- --check`, the extra gate (prints nothing).

- [ ] **Step 5: Commit**

```bash
git add crates/flags-core crates/flags-mcp crates/flags-server/tests/e2e.rs
git commit -m "fix(core): keep server-key errors among those flag_health shows

Error groups were ranked purely by count, and anyone holding the public client
key could report enough error types or spellings to fill the fetch and the
ten shown, hiding what the app's server reported. Groups are now split by the
kind of key that reported them, fetched per reporter, and the five largest
server groups keep their slots. Each group names its reporter.

Fixes #70"
```

---

### Task 4: Final gate run and record as shipped

**Model:** orchestrator (no dispatch).

- [ ] Full gates at HEAD: test, lint, format, extra, `podman build -t otto-flags:check .`.
- [ ] Record pass counts per test binary as evidence.
- [ ] After the PR is open and reviewed: spec `**Status:**` becomes `IMPLEMENTED (#<PR>)`, this
  plan's boxes ticked, in a final commit on the branch before merge:
  `docs: mark the flag_health crowding spec implemented`, body ending `Fixes #69` and `Fixes #70`.
