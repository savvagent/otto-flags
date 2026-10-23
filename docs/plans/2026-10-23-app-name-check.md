# App name CHECK constraint Implementation Plan

**Goal:** the database refuses app names `validate_name` refuses (#157).
**Spec:** docs/specs/2026-10-23-app-name-check-design.md

## Global Constraints

- Gates: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, the
  migrations-append-only diff, `podman build -t otto-flags:check .`.
- New migration file only; worktree only; trunk only via PR; no attribution of any kind.

### Task 1: Migration and test

**Model:** sonnet
**Files:** `crates/flags-core/migrations/0009_flag_apps_name_check.sql`, `crates/flags-core/tests/db.rs`

- [x] Write the failing db test (insert bad names raw; expect `flag_apps_name_check`).
- [x] Add migration 0009; run `cargo test -p flags-core --test db the_database_rejects`; it passes.
- [x] Run every gate; commit `feat(db): enforce the app name rule with a check constraint`, body why, `Fixes #157`.
