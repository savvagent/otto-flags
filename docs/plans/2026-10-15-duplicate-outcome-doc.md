# Outcome::Duplicate doc comment Implementation Plan

**Issue:** #49.

**Goal:** `Outcome::Duplicate`'s doc comment says what a redelivery does for each event kind,
instead of claiming every redelivery re-runs its cleanup (#49). No behaviour change.

**Fast-path:** no spec per general-development trivial-task criteria: one doc comment in one
file, no code change.

**Global constraints:**
- Gates: `DATABASE_URL=postgres://flags:flags@localhost:<port>/otto_flags cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, the
  migrations extra check, and `podman build -t otto-flags:check .` (the change is under `crates/`).
- Worktree only; trunk only via PR. No attribution of any kind.

---

### Task 1: Correct the doc comment

**Files:**
- Modify: `crates/flags-core/src/platform_events.rs` (the `Outcome::Duplicate` variant)

**Interfaces:** none.

**Out-of-band / invariants:** none (comment only; the webhook idempotency invariant is unchanged).

**Model:** haiku — a doc comment matched to the code.

- [x] **Step 1: Check the code** each event's redelivery path runs: `org_deleted` re-runs its
  purge; `member_removed` upserts `removed_members` (refreshing `removed_at`), takes the lock, and
  writes no audit row when the marker already exists; the `TeamDeleted` arm writes only the marker.
- [x] **Step 2: Edit** the comment to say an earlier delivery was already recorded and what the
  redelivery did for each of the three events.
- [x] **Step 3: Run the gates** listed above; all pass.
- [x] **Step 4: Commit**

```bash
git add crates/flags-core/src/platform_events.rs docs/plans/2026-10-15-duplicate-outcome-doc.md
git commit -m "docs(core): say what each redelivery does in Outcome::Duplicate's doc" \
  -m "The comment said every redelivery re-runs its cleanup, which is true only of org.deleted." \
  -m "Fixes #49"
```
