# otto-flags

Feature flags managed by agents over MCP (`/mcp`) and evaluated by running apps through SDKs over
REST (`/api/...`). The server (`crates/`, Rust: axum, sqlx, Postgres 16, rmcp) is a resource server
of the otto platform, which owns identity, OAuth, orgs, teams, and billing; this service holds only
flags. `packages/` holds the SDKs and observability MCP clients, `examples/` the example apps.

Key docs: `VISION.md` (why and what), `docs/deploy/fly.md` (operations, secrets, platform
registration), `docs/SDK-DEVELOPER-GUIDE.md` (the SDK wire contract),
`docs/specs/2026-09-15-otto-flags-design.md` (design, historical),
`docs/plans/2026-10-09-build-and-deploy.md` (v1 decisions and deferred work), and the module docs
at the top of each `crates/*/src/lib.rs`.

## Development Workflow

### Repo
- slug: savvagent/otto-flags
- trunk: main
- tracker: github
- ticketless work: open-issue
- status labels: none

### Tickets
- default assignee: none
- priority labels: none
- estimate labels: none
- types: bug=bug feature=enhancement docs=documentation

### Branch, commit, PR
- branch: <type>/<issue>-<slug>, type = feat | fix | docs | refactor | test | chore
- commit: conventional commits, `<type>(<scope>): <imperative, lower-case>`; scope optional (e.g. `feat(sdk): …`); body explains why and ends `Fixes #N`
- PR body must contain: Fixes #N
- merge: gh pr merge <N> --repo savvagent/otto-flags --admin --squash
- after merge: git push origin --delete <branch>; git branch -D <branch>

The `main` ruleset configures a merge queue that will not enqueue (its `required_deployments`
rule) and auto-merge is disabled, so PRs merge through the org-admin bypass. `--delete-branch` is
refused while the queue is configured, and the repo does not delete merged branches, hence the
manual remote delete. The ruleset requires no status checks, so the bypass enforces nothing: the
merge gate below is on the agent.

### Worktree
- path: .claude/worktrees/<branch with "/" replaced by "-">
- bootstrap:
  - Postgres: `podman ps --format '{{.Ports}}' | grep -q ':15434->'`; if that fails, run `(cd "$(git worktree list --porcelain | sed -n '1s/^worktree //p')" && podman compose up -d)` (compose runs from the main checkout so every worktree shares one container)
  - Role: `psql postgres://flags:flags@localhost:15434/otto_flags -tAc "select 1 from pg_roles where rolname='otto_app'"`; if it prints nothing instead of `1`, run `psql postgres://flags:flags@localhost:15434/otto_flags -c "CREATE ROLE otto_app NOLOGIN;"`
  - If the change touches a path in the pnpm gate below: `pnpm install --frozen-lockfile`
  - If you will run the server locally (the gates do not need it): `cp .env.example .env`, then fill in the platform settings it lists
- teardown: none

No scratch database: `#[sqlx::test]` creates and drops a fresh database per test. Those database
names are derived from the test path alone, so two worktrees running `cargo test` against the same
Postgres at the same time drop each other's databases. Do not run DB tests in parallel worktrees
against one cluster; serialize them, or give a worktree its own container on another port. Port
15433 belongs to otto-factory.

### Gates
- test: DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace
- lint: cargo clippy --workspace --all-targets -- -D warnings
- format: cargo fmt --all -- --check
- extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` must print nothing
- when Dockerfile, .dockerignore, Cargo.toml, Cargo.lock, rust-toolchain.toml, crates/**: podman build -t otto-flags:check .
- when package.json, pnpm-lock.yaml, pnpm-workspace.yaml, packages/angular/**, packages/astro/**, packages/mcp-datadog/**, packages/mcp-dynatrace/**, packages/mcp-newrelic/**, packages/mcp-pagerduty/**, packages/mcp-sdk/**, packages/mcp-sentry/**, packages/mcp-splunk/**, packages/nextjs/**, packages/node-server/**, packages/react/**, packages/remix/**, packages/solid/**, packages/svelte/**, packages/sveltekit/**, packages/typescript/**, packages/vue/**, examples/angular-app/**, examples/astro-app/**, examples/nextjs-app/**, examples/node-backend/**, examples/react-app/**, examples/remix-app/**, examples/solid-app/**, examples/svelte-app/**, examples/sveltekit-app/**, examples/vue-app/**: pnpm install --frozen-lockfile && pnpm lint && pnpm build && pnpm test
- when packages/rust-server/**: cd packages/rust-server && cargo fmt --check
- when packages/rust-server/**: cd packages/rust-server && cargo clippy --locked --all-targets -- -D warnings
- when packages/rust-server/**: cd packages/rust-server && cargo test --locked
- when packages/rust-server/**, examples/rust-server/**: cd examples/rust-server && cargo fmt --check
- when packages/rust-server/**, examples/rust-server/**: cd examples/rust-server && cargo clippy --locked --all-targets -- -D warnings
- when packages/rust-server/**, examples/rust-server/**: cd examples/rust-server && cargo test --locked
- when packages/go-server/**: cd packages/go-server && go vet ./...
- when packages/go-server/**: cd packages/go-server && go test ./...
- when packages/python-server/**: v=$(mktemp -d)/venv && python3 -m venv "$v" && "$v/bin/pip" install -q -e 'packages/python-server[dev]' && cd packages/python-server && "$v/bin/pytest" -p no:cacheprovider && "$v/bin/mypy" --cache-dir=/dev/null src && "$v/bin/ruff" check --no-cache .
- when packages/java-server/**: cd packages/java-server && mvn -B verify
- when packages/ios-sdk/**: cd packages/ios-sdk && swift test
- known red: Go SDK `go vet` and `go test` (no `go.sum`) — #17
- evidence: report pass counts per test binary; `cargo test` without `DATABASE_URL` panics ("DATABASE_URL must be set") rather than skipping, so a panic there is a missing variable, not a test failure; `DATABASE_URL` points only at the local Postgres on 15434, never at a Fly database

The pnpm gate lists exactly the pnpm workspace members (the `packages/*` and `examples/*` dirs with
a `package.json`); root `pnpm lint/build/test` cover `packages/*` only. Known-red gates pass on no
new failures against an `origin/main` baseline; the PR that fixes one removes its entry.
The Go, Java, and Swift gates are unverified here (no toolchain on the dev host). When a gate's
toolchain is missing, do not install one: report the gate as not run, with the reason, in the PR
body. `packages/android-sdk` has no gate (no Gradle wrapper; needs Gradle 8+ and an Android SDK),
nor do `examples/go-server`, `examples/java-server`, `examples/python-server`, `examples/ios-app`,
and `examples/android-app`: verify those with their native tooling and record the gap in the PR
body. The Playwright suite (`examples/e2e`, root `examples/package.json`) targets a retired API and
has no gate (see `sdks.yml`).

### Design docs
- mode: committed
- spec path: docs/specs/YYYY-MM-DD-<slug>-design.md
- plan path: docs/plans/YYYY-MM-DD-<slug>.md
- shipped: final commit in the same PR before merge: the spec's `**Status:**` line becomes `IMPLEMENTED (#<PR>)` and the plan's task boxes are ticked. Never a follow-up PR: every merge to `main` is a production deploy.

### Reviews
- language reviewers: crates/** -> rust-pro, packages/rust-server/** -> rust-pro, examples/rust-server/** -> rust-pro, packages/go-server/** -> golang-pro, packages/python-server/** -> python-pro, packages/java-server/** -> java-pro, packages/ios-sdk/** -> ios-developer, packages/android-sdk/** -> mobile-developer, examples/go-server/** -> golang-pro, examples/python-server/** -> python-pro, examples/java-server/** -> java-pro, examples/ios-app/** -> ios-developer, examples/android-app/** -> mobile-developer, packages/** -> typescript-pro, examples/** -> typescript-pro
- extra reviewers: database-optimizer when SQL, migrations, indexes, or rollups change
- security focus: tenant isolation (RLS under `otto_app`, composite foreign keys, org or app ids trusted from the request); bearer-token audience, expiry, and scope checks; owner/admin checks on app creation, key rotation, and opting a rollback policy into client reports; SDK key resolution and what a public `sdk_` client key can reach; secret and key handling (hashing, logging, echoing, timing-safe compare); platform webhook signature verification and idempotency; untrusted input (SQL built from strings, unbounded bodies, SDK telemetry text reaching an agent, database error text in responses); cheap unauthenticated requests on a small shared database; persisted end-user context or PII
- automated reviewer: none

First match wins per file, so the per-language globs come before the `packages/**` and
`examples/**` catch-alls, which then cover only the JS/TS packages and example apps. No review bot
has reviewed or commented on any PR here (#3, #6, #8, #10).

### Ship
- deploy: ci-auto
- merge gate: on the PR's head SHA: `rust`, `docker-build`, `migrations-append-only` (CI workflow) succeeded; `Test` (SDKs workflow) succeeded when any changed file matches `packages/**`, `examples/**`, `package.json`, `pnpm-lock.yaml`, or `pnpm-workspace.yaml` (the workflow's own path filter); `Rust SDK` (Rust SDK workflow) succeeded when any changed file matches `packages/rust-server/**`, `rust-toolchain.toml`, or `.github/workflows/rust-sdk.yml`; plus every other check that ran on the head SHA (the ruleset requires none)
- verify: the SHA proxy is the `deploy` job of the CI run for the merge SHA (`gh run list -R savvagent/otto-flags --workflow CI --commit <merge SHA> --json databaseId,status,conclusion`, then `gh run view <id> -R savvagent/otto-flags --json jobs`); `/readyz` does not report a SHA. Then `fly releases -a otto-flags` shows a new release; `fly status -a otto-flags` shows the `app` machines on it; `curl -s https://otto-flags.savvagent.com/readyz` returns `{"status":"ready"}` (the first request may resume a suspended machine); `fly logs -a otto-flags --no-tail` shows `applying migrations`, `tenant isolation enforced as role "otto_app"`, `the otto platform accepted this service's credential`, and `declared usage meters` (without it no usage ships; see `docs/deploy/fly.md`, Metering); then exercise the changed surface read-only (`/mcp` tools via an MCP client, `whoami` first, or the changed `/api/...` route). A change outside `crates/`, `Dockerfile`, `fly.toml`, and the root `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` (SDKs, examples, docs, workflows) redeploys unchanged server code: verify health only and state that the changed surface is not deployed. A machine failing `/readyz` fails the deploy: merged, not live. Do not assume Fly rolled it back: `fly status` must show no `app` machine left on the failed release (moving one back is a hand deploy, Rob's; `docs/deploy/fly.md`, When a migration fails). A previous release boots against a newer schema because `MIGRATOR` ignores migration versions it does not know (`crates/flags-core/src/migrate.rs`).
- release: server: none (continuous deploy of every push to `main`). Changesets apply to pnpm packages only: a change to a published pnpm package (`packages/*` with a `package.json`) adds a changeset (`pnpm changeset`); docs- and test-only changes do not. npm publishing is not automated (`.github/workflows/release.yml` triggers on `master`, which does not exist) and no `@otto-flags/*` package is on npm, so a merged SDK change is merged, not published. The Rust crate (`packages/rust-server`, `otto-flags`), Go, Python, Java, iOS, and Android SDKs are not published (not on crates.io, PyPI, Maven Central, or the Go proxy; the Go module path names a repo, `savvagent/otto-flags-go-server-sdk`, that does not exist); a release is a human decision (Rob). Do not bump their versions in a change PR.
- version files: packages/*/package.json, packages/rust-server/Cargo.toml, packages/python-server/pyproject.toml, packages/java-server/pom.xml, packages/android-sdk/build.gradle.kts

### Out-of-band
- Migrations (agent): apply at boot under an advisory lock; confirm in the boot logs. A failure rolls back that migration only (earlier ones in the same boot stay applied) and crash-loops the new machine; check `fly status` as under Ship > verify. The previous release boots against whatever newer migrations did apply: `MIGRATOR` ignores unknown versions (`crates/flags-core/src/migrate.rs`). Recovery, including re-running a deploy (Rob's call), is in `docs/deploy/fly.md` (When a migration fails).
- New non-secret config key (agent, in the PR): `fly.toml` `[env]`, `.env.example`, and `crates/flags-server/src/config.rs`.
- New secret (human: Rob, before merge): `fly secrets set -a otto-flags --stage NAME=…`; the PR updates `.env.example` and the secrets table in `docs/deploy/fly.md`.
- New scope (human: Rob, before merge): the PR updates `flags_core::scopes::KNOWN`/`DEFAULT`; Rob re-runs the platform `otto-platform-server resource register …` from `docs/deploy/fly.md`.
- otto-platform pin (agent, own PR): `otto-tenant` and `otto-resource` move to the same rev together.
- npm publish (human: Rob): not automated; see Ship > release.

### Human-only
Hand these to Rob with the exact command; never run them:
- Any production write, DDL, `DROP`, or role change on `otto-db` (the shared Fly Postgres also
  hosting `otto_platform` and `otto_factory`; the attach role is superuser on the whole instance).
  Production diagnostics are SELECT-only.
- `fly secrets set`, and platform-side `otto-platform-server resource …` commands.
- A hand `fly deploy`, or re-running a failed CI `deploy` job (only when Rob asks).
- Anything that adds recurring cost: a bigger VM, `min_machines_running > 0`, a new volume, app,
  database, or paid service.
- Package publishes.

### Invariants
Reviewers check every one. Details: `docs/deploy/fly.md`, `docs/SDK-DEVELOPER-GUIDE.md`, and the
crate module docs.
- **Append-only migrations.** A schema change is a new `crates/flags-core/migrations/NNNN_<name>.sql`.
  Never modify, rename, or delete an existing one, not even a comment, and never reuse a version
  number, even one only a rolled-back release applied: sqlx checksums applied migrations and the
  next deploy refuses to boot on a modified one. `MIGRATOR` ignores unknown versions, so only CI's
  `migrations-append-only` job catches a delete or rename (the contiguity test in
  `crates/flags-core/src/migrate.rs` also catches a gap or a duplicate number).
- **Migrations must be compatible with the previous release during rollout AND after a rollback:**
  the previous release runs against the newer schema (`ignore_missing`,
  `crates/flags-core/src/migrate.rs`). Additive only: new columns nullable or defaulted; no drop,
  rename, retype, or new constraint that its reads or writes would trip.
- **Tenant isolation is RLS under `otto_app`, proven.** Tenant queries are extension-trait methods
  on `otto_tenant::Tx` in `flags-core`. Every new tenant table enables RLS, grants to `otto_app`,
  and is covered by `tenant_isolation_is_enforced_on_every_tenant_table`
  (`crates/flags-core/tests/db.rs`); `flags-server` refuses to start if isolation fails at boot.
- **Composite `(org_id, id)` foreign keys** between tenant tables (FK checks bypass RLS).
  `app_keys` and `deleted_orgs` are the deliberate pre-org exceptions.
- **Request paths open transactions with `platform_events::begin_live`**, never `Db::begin`.
  Unpinned cross-org access goes only through `flags_core::maintenance::begin` (background code:
  sweeps, the rollback checker's due-policy scan, the usage shipper, the evaluation meter's
  flush and hour close), which runs as `otto_app`
  when the pool can assume it, else as the pool role only if RLS applies to it, refusing
  otherwise, and sets the `app.maintenance` marker the unpinned RLS policies require.
  Other unpinned queries (key resolution, revocation, tombstones) go through
  `flags_core::unpinned::begin`: the same role drop, no marker. A new raw `.pool()` query fails
  `no_raw_pool_queries_outside_the_allowed_sites` (`crates/flags-core/tests/db.rs`).
- **No SQL outside `flags-core`**, and only runtime `sqlx::query`/`query_as`, never `query!`
  macros (the Docker build has no database).
- **Client keys are public.** `sdk_` and `srv_` keys are stored as SHA-256 hashes; a client key
  never sees targeting rules, attribute names, or other orgs' data.
- **The SDK path never calls the platform.** `/api/...` resolves keys against this database only.
- **The SDK REST contract is stable** (`docs/SDK-DEVELOPER-GUIDE.md`); changes are additive.
  SDKs fall back to defaults and cached values and never throw into the host app on a flag lookup.
- **MCP auth.** Token audience equals `FLAGS_RESOURCE_URI` exactly; every tool checks its scope
  (`flags:read`, `flags:write`, `apps:admin`); app creation, key rotation, and setting a rollback
  policy's `includeClientReports` do a fresh owner/admin lookup at the platform. `flags_core::scopes::KNOWN`/`DEFAULT` match the platform
  registration.
- **Metering:** MCP tools record a free usage row in their own transaction (nothing an agent does
  is billed). Production evaluations are counted in memory (`flags_core::metering::EvalCounter`),
  flushed to `usage_meter_hourly`, closed hourly into `usage_outbox` on `otto-flags/evaluations` as
  `GREATEST(served, reported)` (`reported` = server-key telemetry only), unmetered while the org is
  `billable_blocked`. The SDK path only touches the counter. Reads and dry-runs are never billed.
- **Every flag change is versioned** in `flag_versions` and calls `pg_notify('flag_changes', …)`
  in the same transaction.
- **Evaluations are counted, not stored:** `flag_eval_hourly` rolls up SDK telemetry only (never
  the evaluate endpoint); error reports are raw and truncated (14 days; rollups 90); no end-user
  context is persisted. `usage_meter_hourly` holds billing counts per app and hour only.
- **SDK-reported text is untrusted** when an MCP tool returns it; database error text never
  reaches a tool response.
- **Rollout bucketing** (SHA-256 of flag key + identifier in `flags_core::eval`) never changes
  incidentally; changing it reshuffles every live rollout.
- **Platform webhooks** (`org.deleted`, `member.removed`, `team.deleted`) are signature-verified,
  fail closed, and apply idempotently.
- **The database is small and shared:** 5 pool connections on a 512 MB instance. No unbounded
  tables, per-row query fan-out, or pool connection held per SSE stream.
- **Licensing:** `crates/` and repo-level files are AGPL-3.0-or-later (`LICENSE`, `NOTICE`);
  `packages/` and `examples/` are MIT, each package with its own `LICENSE` and
  `"license": "MIT"`. Never copy server code into an MIT package.
- **otto-platform pin:** `otto-tenant` and `otto-resource` share one git rev in the root
  `Cargo.toml`, bumped together and deliberately, never tracking a branch.

## Notes
- Rust: stable toolchain (`rust-toolchain.toml`), MSRV 1.88, edition 2021. Workspace members are
  `crates/flags-{core,mcp,api,server}`; `packages/rust-server` and `examples/rust-server` are
  excluded standalone crates, each its own workspace root (an empty `[workspace]` table, so
  cargo works on them in place, worktrees included) with its own `Cargo.lock`. Server
  dependencies go in the root `[workspace.dependencies]`, committed with `Cargo.lock`.
- Test placement: SQL and domain behavior in `crates/flags-core/tests/db.rs` with
  `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`; pure logic as unit tests beside the code;
  MCP, SDK API, and webhook behavior in `crates/flags-server/tests/e2e.rs` against its mock platform.
  A test that captures `tracing` output gets its own `tests/` binary (callsite-interest caching;
  see `crates/flags-mcp/tests/failure_logs.rs`).
- The JS/TS packages have CI in `.github/workflows/sdks.yml` and the Rust SDK in
  `.github/workflows/rust-sdk.yml`. The Go, Python, Java, Swift, and Kotlin SDKs have none; their
  local gates are under Gates.
- `CONTRIBUTING.md` predates the server and covers only the SDKs: its `feature/` branch prefix,
  automated-release description, and "MIT for all contributions" line do not match this section,
  `release.yml`, or `NOTICE`. This section wins.
