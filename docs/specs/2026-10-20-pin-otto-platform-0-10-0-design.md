# Pin otto-platform 0.10.0

**Status:** DRAFT
**Ticket:** savvagent/otto-flags#140 (first of two PRs; the second meters evaluations)

## Brief

#140 says: "Bump `otto-tenant`/`otto-resource` to the platform release with meter, quantity and
`budget_exceeded`, and declare the meter at startup." The ticket's comment adds that otto-platform
v0.10.0 (savvagent/otto-platform#84) ships `MeterDeclaration`, `PlatformClient::declare_meters`,
`service`/`meter`/`quantity` on `UsageEvent`, and `meters`/`budget`/`blocked_reason` on
`UsageStatus`. It also notes that Rust code building `UsageEvent` as a struct literal needs the new
fields.

The Development Workflow section (Out-of-band) says the otto-platform pin moves in its own PR. This
spec covers that PR only. Metering lands in the next PR.

## Premise corrections

None.

## Assumptions

- The Run Profile is complete in CLAUDE.md; no key was assumed.
- Dependency changes are not fast-path eligible, hence this spec.
- v0.10.0 is rev `bfda55e155be8985761850a7c42980cc926fd4cb` (tag v0.10.0). It was deployed to
  production on 2026-10-21 (Fly v14), so this service already talks to a v0.10.0 platform.

## Goal and success criteria

Land the v0.10.0 pin with no behavior change, so the metering PR can build on the new types.

- Both crates pin one rev: `bfda55e155be8985761850a7c42980cc926fd4cb`.
- `Cargo.lock` changes only `otto-tenant` and `otto-resource` (0.5.0 to 0.10.0), plus the `hmac`
  and `sha2` dependency edges `otto-tenant` gained. Both are already in the lock.
- Usage events ship exactly as before: `service`, `meter` and `quantity` are `None`, and serde
  skips them on the wire.
- The workspace builds, and tests, clippy, fmt, the migrations-append-only check and the container
  build all pass.

## Scope

- **In:**
  - The `Cargo.toml` rev.
  - `Cargo.lock`.
  - The `UsageEvent` literal in `crates/flags-core/src/usage.rs` (`ship_once`).
- **Out:**
  - Meters, declarations, and any use of `budget` or `blocked_reason` (the next PR).
  - Any migration.

## Architecture

The library diff between `fa1ae08c` and `bfda55e` under `crates/otto-(resource|tenant)/src` touches:

- `otto-resource`: `client.rs`, `lib.rs` and `types.rs` (new types, `declare_meters`, three
  optional `UsageEvent` fields, new `UsageStatus` fields with serde defaults).
- `otto-tenant`: `crypto.rs` (key derivation; not used here) and `db.rs` (`otto_tenant::db::MIGRATOR`
  becomes `LazyLock` with `ignore_missing`; not used here, since this service runs only
  `flags_core::MIGRATOR`).

The only compile break is the `UsageEvent` struct literal, which gets `service: None, meter: None,
quantity: None`.

`UsageStatus::is_blocked()` is documented as always false against a current platform. The
production platform already answers that way, so `FLAGS_ENFORCE_QUOTAS` (off in production) already
cannot refuse anything. Nothing changes. The next PR retires the variable.

No public interface of this service changes.

## Invariants impact

- **otto-platform pin:** both crates move together to one rev, never a branch.
- **Append-only migrations and tenant isolation:** the platform's migrations 0017–0023 belong to
  `otto-tenant` and are never applied to `otto_flags`. No schema change here; the RLS proof is
  unchanged.
- **Metering:** recording is unchanged. Rows ship without a meter, as today.
- **Licensing:** no code is copied.

## Out-of-band

None: no new secret, config key or scope. The merge redeploys the rebuilt server (ci-auto).

## Error handling and edge cases

A rev that cannot be fetched fails the build. The container-build gate covers this.

## Testing approach

- The full `cargo test --workspace` against local Postgres on 15434.
- clippy, fmt, the migrations-append-only check, and `podman build -t otto-flags:check .`.

The e2e platform mock answers `usage_status` as JSON without the new fields, which proves that
their serde defaults hold against an older answer shape.

## Risks

The platform is already at v0.10.0 in production, so wire compatibility is the intended state.
`declare_meters` is unused until the next PR.
