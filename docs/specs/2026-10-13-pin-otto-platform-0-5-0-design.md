# Pin otto-platform 0.5.0

**Status:** IMPLEMENTED (#30)
**Ticket:** savvagent/otto-flags#25

## Brief

"Bump the otto-platform pin to 0.5.0 in its own PR, ahead of PR #24 (auto-rollback), which needs
`otto_resource::UsageStatus::features`. otto-platform 0.5.0 is released and deployed (tag v0.5.0,
rev fa1ae08c8d5760c1df9752281a078192032e7d24, deployed 2026-10-10)."

## Premise corrections

None. The compare of a5d169f (0.4.0) to fa1ae08 (0.5.0) touches exactly two files under
`crates/otto-(resource|tenant)/src/` or `migrations/`: `otto-resource/src/types.rs` (the library API
change) and `otto-tenant/migrations/0016_plan_features.sql`.

## Assumptions

- The Run Profile is complete in CLAUDE.md; no key was assumed.
- Dependency changes are not fast-path eligible, hence this spec.
- `features` has no consumer here yet; the mock sets `Default::default()`.

## Goal and success criteria

Land the 0.5.0 pin so #24 can build on `UsageStatus::features`.

- Both crates pin one rev, fa1ae08c8d5760c1df9752281a078192032e7d24.
- `Cargo.lock` changes only `otto-tenant` and `otto-resource`, 0.4.0 to 0.5.0.
- Workspace builds, tests, clippy, fmt and the Docker build pass.

## Scope

In: `Cargo.toml` rev, `Cargo.lock`, the `usage_status` mock in `crates/flags-server/tests/e2e.rs`.
Out: any use of `features` in product code (#24); any migration.

## Architecture

`UsageStatus` gains a `features` field, so the struct literal in the e2e mock
(`crates/flags-server/tests/e2e.rs`, `usage_status`) needs `features: Default::default()`. No public
interface of this service changes.

## Invariants impact

- otto-platform pin: both crates move together to one rev, never a branch.
- Append-only migrations / tenant isolation: `otto-tenant/migrations/0016_plan_features.sql` is the
  platform's own and is never applied to otto_flags; `flags_core::migrate` runs only
  `flags_core::MIGRATOR`. The pin therefore adds no schema change here. The RLS proof and the
  boot-time isolation check are unchanged.
- Licensing: no code is copied.

## Out-of-band

None: no new secret, config key, or scope. Merge deploys rebuilt server code (ci-auto).

## Error handling and edge cases

A rev that cannot be fetched fails the build (Docker build gate covers it).

## Testing approach

Full `cargo test --workspace` against local Postgres (serialized with a lock), clippy, fmt, the
migrations-append-only check, and `podman build -t otto-flags:check .`.

## Risks

0.5.0 behavior changes beyond `types.rs` are excluded by the compare; the platform is deployed at
0.5.0 already, so wire compatibility is the intended state.
