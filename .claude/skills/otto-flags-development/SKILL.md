---
name: otto-flags-development
description: Use when developing any feature or fix in the otto-flags repository (the MCP-managed feature-flag service on the otto platform — a Rust/axum/sqlx server in crates/ plus the MIT TypeScript SDKs in packages/) end-to-end — from a GitHub issue or plain task brief, through ship, deploy, and verify, fully autonomously with no mid-run questions. Bundles the spec-and-plan discipline (committed docs in docs/specs/ and docs/plans/), this repo's tenant-isolation spine (RLS under otto_app, begin_live, no SQL outside flags-core), the append-only sqlx migration contract, the stable SDK REST contract, autonomous spec and plan generation, task-by-task implementation, PR review loops, the merge-deploys-to-Fly pipeline, verification, and close-out. For other repositories use general-development.
---

# otto-flags Development — Autonomous End-to-End

Autonomous, plan-driven feature/fix workflow for the **otto-flags** repository: feature flags managed
by agents over MCP and evaluated by running apps over a REST SDK API, as a **resource server of the
otto platform** (identity, OAuth, orgs, teams, and billing live in otto-platform; this service holds
only flags). Walks from intake (a GitHub issue OR a plain task brief) through spec → plan →
implement → PR → review → merge → deploy → verify → close, with no mid-run human questions. Returns
control only when the work is shipped + verified, or on a true blocker.

This is the **otto-flags-specific sibling** of `general-development`. Same spine; the repo-agnostic
convention-discovery phase is replaced by the hardcoded conventions below (Cargo workspace + pnpm
workspace, committed specs and plans, the tenant-isolation spine, the append-only migration
contract). If you are working in any other repository, use `general-development`.

## Why this shape

The repo is built spec-first: `docs/specs/2026-09-15-otto-flags-design.md` is the product design and
`docs/plans/2026-10-09-build-and-deploy.md` records the v1 decisions (resource-server model, hashed
SDK keys, counted-not-stored evaluations, versioned flags, scopes, metering, realtime). The workflow
below makes that habit explicit and autonomous — a spec first, a plan against it, implementation
against the plan, and the repo's own CI (`.github/workflows/ci.yml`) as the verifier.
`VISION.md` is the product direction, `docs/deploy/fly.md` is the operations record, and
`docs/SDK-DEVELOPER-GUIDE.md` is the SDK wire contract the published packages depend on.

## The Iron Law

**The source of truth is the GitHub issue.** Every PR in this repo references the issue it resolves
(a standing rule of the repo owner). If the work began from a plain instruction, capture it verbatim
and open an issue from it at intake (Phase 1 step 1) — that issue is the contract from then on. Not
Slack. Not the PR title. Not a teammate's summary.

**Fully autonomous means no mid-run questions.** Make the most reasonable interpretation, document
the assumption, continue. Escalate only on true blockers. "Should I continue?" is never a stop
condition.

**Green tests are not the same as work-done.** In this repo that sentence has three teeth:

1. `cargo test` is not the whole gate. CI also runs `cargo fmt --check` and
   `cargo clippy --workspace --all-targets -- -D warnings` (any warning fails), refuses edits to an
   existing migration, and builds the Docker image — `cargo test` can pass while the image does
   not. Run all of it locally (Phase 3 step H).
2. The tests prove isolation only for tables they cover. `verify_tenant_isolation` (proven at boot
   and in `crates/flags-core/tests/db.rs`) checks every tenant table runs under RLS as `otto_app`;
   a new table that skips RLS or the composite `(org_id, id)` foreign keys is a cross-tenant leak no
   behavior test will notice.
3. **Merging to `main` deploys to production.** Every green push to `main` runs the `deploy` job in
   `ci.yml` (`flyctl deploy --remote-only -a otto-flags`), and migrations run at boot. "Merged" is
   therefore "deploying" — but not "verified": the platform registration, Fly secrets, the shared
   database, and the live host are all outside the test suite (Phase 5).

**Violating the letter of the workflow is violating the spirit.**

## Non-Negotiable Rules

These hold for every run of this skill, no exceptions, no fast-path carve-outs:

1. **All work happens in a worktree.** Never edit, commit, or stage anything in the main checkout's
   working tree. The worktree is created in Phase 0 and removed in Phase 4 step 12.
2. **`main` changes only through PRs.** Every commit to `main` lands via a reviewed, merged PR that
   references its issue. Never `git push origin main`; never merge without the PR open, reviewed,
   and green. A push to `main` is a **production deploy**.
3. **Coding agents never self-attribute.** No `Co-Authored-By` trailers, no "Generated with"
   footers, no `🤖`/`AI`/credit markers of any kind — in commit messages, PR bodies, issue bodies,
   code comments, docs, or READMEs. This applies to direct work and to anything delegated to a
   subagent. An otherwise-perfect commit that carries attribution is rejected and rewritten.
4. **Every PR is reviewed by a language expert, an architect, and an independent security agent.**
   `rust-pro` for any change under `crates/` (and `typescript-pro` for any change under
   `packages/` or `examples/` — both when both change), `architect-reviewer`, and
   `security-auditor`. No size-based carve-out. All must pass (or their issues be resolved) before
   merge.
5. **The security review is independent.** The `security-auditor` agent receives **only the PR
   diff** — never the spec, plan, issue, PR body, or implementer's report — so it cannot be steered
   by the implementer's framing.
6. **Migrations are append-only.** A schema change is a **new** file in
   `crates/flags-core/migrations/` (`NNNN_<snake_name>.sql`, next number in sequence). Never modify,
   rename, or delete an existing migration — not even a comment: sqlx stores each applied
   migration's checksum and refuses to start against a database whose files no longer match, so an
   edited migration takes down the next deploy. CI's `migrations-append-only` job refuses it. Every
   new tenant table enables RLS, grants to `otto_app`, and uses composite `(org_id, id)` foreign
   keys like the baseline (foreign-key checks bypass RLS).
7. **Production is read-only from here, and its Postgres instance is shared.** `otto_flags` lives
   on `otto-db`, an unmanaged 512 MB Fly Postgres shared with `otto_platform` and `otto_factory`,
   where the attach role is a superuser on the whole instance. Never point `DATABASE_URL` for tests
   at it — `#[sqlx::test]` creates and drops databases. Never run DDL, `DROP`, or role changes
   there. Production diagnostics are SELECT-only. **Production writes, `fly secrets set`, and
   platform-side `resource …` commands are handed to Rob to run, not executed here.**
8. **No new recurring cost without Rob's say-so.** Anything that adds a paid resource (a bigger VM,
   `min_machines_running > 0`, a new volume, app, database, or paid service) is an escalation, not
   an assumption.

## When to Use This Skill vs. Alternatives

| Situation | Use |
|---|---|
| Any feature/fix in otto-flags, full lifecycle, no human in the loop | **otto-flags-development** (this skill) |
| Work in another repository | `general-development` |
| Already mid-implementation, just need to address PR review comments | the Review-Response step here (Phase 4 step 9) |
| Spec/plan only, will hand off to a human implementer | Phases 1–2 of this skill |
| Guided mode with human approval at each checkpoint | run the phases directly, stopping at each gate |
| One-line typo fix or docs nit | Fast-Path below — the spec phases are overkill |

## Fast-Path: Trivial Tasks (skip the spec + critique loops)

Skip the spec document, the spec critique, and the plan critique ONLY when **ALL** of the following
are true:

- 1–2 logical source files (tests, `Cargo.lock`, and `pnpm-lock.yaml` don't count toward the cap)
- **No migration** and no change to any SQL in `crates/flags-core`
- No new public surface: no new MCP tool or tool parameter, no new or changed SDK REST route,
  request, or response field, no new config key in `crates/flags-server/src/config.rs`, no new
  scope, no new crate or package, no new dependency
- No change to auth or isolation: `begin_live`, `otto_tenant` usage, key hashing or resolution,
  introspection, scope checks, the owner/admin lookup, webhook signature verification, or what a
  client (`sdk_…`) key may see
- No change to metering, the evaluation engine's semantics (`flags_core::eval`), flag versioning,
  or telemetry counting
- No change to deploy/distribution shape (`fly.toml`, `Dockerfile`, `compose.yaml`,
  `.github/workflows/`, the otto-platform pin in `Cargo.toml`)
- No behavior change on a code path covered by tests (a doc comment or log-message fix is fine)
- The acceptance criterion fits in one sentence

Concrete examples that qualify: a typo in a comment, doc, or README; a doc comment that misstates
what the code does; a dead-code removal with zero callers; a constant the issue names verbatim.

**Even when fast-pathing, the plan document is not skipped.** Land a minimal single-task plan at
`docs/plans/YYYY-MM-DD-<slug>.md` (Goal + one `### Task` with `- [ ]` steps + test + commit step).
Add to the PR body: `Fast-path: no design spec per otto-flags-development trivial-task criteria — <reason>.`

| Fast-path rationalization | Reality |
|---|---|
| "It's only 3 files" | Fast-path caps at 2. Three files → spec. |
| "It's a tiny additive migration" | Any migration is Rule 6 and production DDL. Spec it. |
| "It's one more field in the SDK response" | The SDK contract is consumed by published packages. Spec it. |
| "It's just one config key" | A config key is a deploy-time env var or secret. Spec it. |
| "The fix incidentally changes behavior" | Then the spec records what changed and why. |

## Repository Conventions (otto-flags)

| Convention | Value |
|---|---|
| Repo | `savvagent/otto-flags` (GitHub) |
| Trunk | `main`. A **merge queue** is configured, but it refuses to enqueue (the ruleset's `required_deployments` rule) and auto-merge is disabled, so PRs merge with Rob's admin bypass: `gh pr merge <N> --admin --squash` (Phase 4 step 11). Every green push to `main` deploys. |
| Worktree | **Required.** `git worktree add .claude/worktrees/<dir> -b <branch> origin/main` — worktrees live inside the repo at `.claude/worktrees/<dir>` (gitignored; `<dir>` is the branch name with `/` → `-`). Branch off `origin/main`, never local `main`. |
| Worktree bootstrap | Nothing to copy: tests need only `DATABASE_URL`, and `#[sqlx::test]` creates a fresh database per test, so worktrees can share the local Postgres. A local server run needs `.env` (copy `.env.example`; it is gitignored). |
| Branch name | `fix/<kebab-slug>`, `feat/<kebab-slug>`, `docs/…`, `refactor/…`, `test/…` (per `CONTRIBUTING.md`) |
| Commit format | **Conventional commits**: `fix: …`, `feat: …`, `docs: …`, optional scope (`feat(sdk): …`). Lower-case after the prefix, imperative, describes the behavior. Body explains *why*. Body ends `Fixes #N`. Squash-merge appends `(#N)`. |
| AI attribution | **Never** (Non-Negotiable Rule 3). |
| Issue linkage | **Every PR references its issue** (`Fixes #N` in the body). Ticketless work gets an issue opened at intake. |
| Spec storage | **Repo file, committed.** `docs/specs/YYYY-MM-DD-<slug>-design.md`. |
| Plan storage | **Repo file, committed.** `docs/plans/YYYY-MM-DD-<slug>.md`. |
| Licensing | `crates/` and repo-level files are **AGPL-3.0-or-later** (`LICENSE`, `NOTICE`); `packages/` and `examples/` are **MIT** (a `LICENSE` file in each). A new package ships its own MIT `LICENSE` and `"license": "MIT"`; never copy server code into an MIT package. |
| Rust toolchain | stable (`rust-toolchain.toml`), MSRV 1.88, edition 2021, workspace members `crates/flags-{core,mcp,api,server}`. `packages/rust-server` and `examples/rust-server` are excluded standalone crates. |
| Server gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`. |
| Local database | `podman compose up -d` (from the main checkout) starts Postgres 16 on host port **15434** (user/pass `flags`/`flags`, db `otto_flags`, a superuser, container `otto-flags-postgres-1`). The `otto_app` role must exist (`CREATE ROLE otto_app NOLOGIN;` once — CI pre-creates it to avoid a race between parallel test databases). Port 15433 is otto-factory's; leave it alone. |
| SDK gates | Only when `packages/`, `examples/`, or the pnpm files change: `pnpm install --frozen-lockfile && pnpm lint && pnpm build && pnpm test` (CI: `.github/workflows/sdks.yml`). A change to a published package adds a changeset (`pnpm changeset`); docs/test-only changes don't. |
| Lint / format | Rust: rustfmt + clippy `-D warnings`. TS: each package's `lint` script. Do not add new tools as a side-effect of a feature PR. |
| CI | `ci.yml`: `rust` (fmt, clippy, test against Postgres), `migrations-append-only` (PRs), `docker-build`, `deploy` (pushes to `main` only, after `rust` + `docker-build`). `sdks.yml`: path-filtered SDK lint/build/test. `release.yml` targets a non-existent `master` branch, so npm publishing is **not** automated — do not claim a package was released. |
| Deploy | Automatic on merge: CI `deploy` job → `flyctl deploy --remote-only -a otto-flags`. Hand deploy: `fly deploy -a otto-flags --remote-only` (only when Rob asks). App `otto-flags`, org `savvagent`, region `iad`, shared-cpu-1x / 256 MB, suspends when idle. Public host `https://otto-flags.savvagent.com`; MCP at `/mcp`; readiness `/readyz`. |
| Platform dependency | `otto-tenant` and `otto-resource` are pinned to **one** otto-platform git rev in the root `Cargo.toml`. Bump both together, deliberately, in their own PR — never track a branch. |

Full reference: `README.md`, `docs/deploy/fly.md`, the design spec and v1 plan above, and the module
docs at the top of each crate's `lib.rs`.

## Load-Bearing Invariants (the "isolation spine" — get these right)

These are not style rules; every review step below checks them.

1. **Tenant isolation is RLS under `otto_app`, and it is proven, not assumed.** Every tenant query
   is a method on an `otto_tenant::Tx` (which cannot exist without an org), written as an extension
   trait in `flags-core` (`AppsExt`, `FlagsExt`, …). `otto_tenant::Db::begin` issues
   `SET LOCAL ROLE otto_app`; `flags-server` proves isolation at boot and
   `tenant_isolation_is_enforced_on_every_tenant_table` proves it in tests. A new tenant table
   enables RLS and is covered by that check.
2. **Request paths open transactions with `platform_events::begin_live`, never `Db::begin`
   directly**, so a deleted org or removed member is refused.
3. **No SQL outside `flags-core`.** `flags-mcp`, `flags-api`, and `flags-server` call domain
   methods. Every statement is a runtime `sqlx::query`, **never a `query!` macro** — the Docker
   build has no database.
4. **Composite `(org_id, id)` foreign keys** on every reference between tenant tables: foreign-key
   checks bypass RLS, so a plain `id` FK would let a row point at another org's app or flag.
   Lookup tables that must resolve before the org is known (`app_keys`, `deleted_orgs`) are the
   deliberate exceptions and stay outside RLS.
5. **SDK keys.** `sdk_…` (client) keys are public; `srv_…` (server) keys are secret. Both are stored
   as SHA-256 hashes in `app_keys`; the server key is shown once, at creation or rotation. **A
   client key never sees targeting rules** — anything returned to a client key must not leak rules,
   attribute names, or other orgs' data.
6. **The SDK path never calls the platform.** `/api/...` resolves keys against this database alone,
   so a platform outage does not affect evaluation. Never add an introspection, member lookup, or
   other platform call on the SDK path.
7. **The SDK REST contract is stable.** `docs/SDK-DEVELOPER-GUIDE.md` defines it and the published
   packages in `packages/` depend on it (`POST /api/flags/{key}/evaluate` plus the Rust SDK's
   `POST /api/evaluate/{key}`, `GET /api/sdk/flags`, `GET /api/sdk/enterprise-flags`,
   `GET /api/flags/stream` (SSE), `POST /api/telemetry/evaluations`, `POST /api/telemetry/errors`;
   `Authorization: Bearer` or `X-SDK-Key`). Changes are additive; a breaking change is an
   escalation.
8. **MCP auth.** Bearer tokens are introspected at the platform (`otto-resource`, cached 60 s), the
   audience must equal `FLAGS_RESOURCE_URI` exactly, and every tool checks its scope
   (`flags:read`, `flags:write`, `apps:admin`; default `flags:read`). App creation and key rotation
   also require a **fresh** owner/admin member lookup at the platform (the lesson from
   otto-factory#200). `flags_core::scopes::KNOWN`/`DEFAULT` must match the platform registration
   — a new scope is an out-of-band step (Phase 5).
9. **Metering.** MCP writes are billable, recorded in the tool's **own transaction** into
   `usage_outbox`, which a background shipper sends to the platform. Reads and dry-run evaluation
   are free. The SDK evaluation path is not metered.
10. **Every flag change is versioned** in `flag_versions` (full snapshot, actor, reason) so
    `rollback_flag` can restore any version, and writes `pg_notify('flag_changes', …)` **in the
    same transaction** so SSE subscribers see only committed changes.
11. **Evaluations are counted, not stored.** Telemetry folds into hourly rollups
    (`flag_eval_hourly`), counted from SDK telemetry only — never on the evaluate endpoint (SDKs
    report every evaluation, so counting both double-counts). Error reports are kept raw,
    truncated, for 14 days. Never persist end-user evaluation context.
12. **Text from SDKs is untrusted when an agent reads it.** Error text reported through telemetry is
    flattened, shortened, and labelled untrusted before an MCP tool returns it; database error text
    never reaches a tool response (`flags-mcp` error tests). Keep both.
13. **Evaluation semantics** (`flags_core::eval`, pure): archived or disabled → off; rules in order,
    first match wins; otherwise percentage rollout bucketed deterministically by SHA-256 of
    `flag key + identifier` (user_id → anonymous_id → session_id). Changing bucketing reshuffles
    every live rollout — an escalation, never an incidental refactor.
14. **Webhooks from the platform** (`/platform/webhooks`: `org.deleted`, `member.removed`,
    `team.deleted`) are signature-verified with `FLAGS_PLATFORM_WEBHOOK_SECRET` and applied
    idempotently. Verification fails closed.
15. **The shared database is small.** `FLAGS_DB_MAX_CONNECTIONS = 5` on a 512 MB instance shared
    with two other apps. Unbounded tables, per-row query fan-out, and long-held connections (SSE
    must not hold a pool connection per stream; there is one `PgListener` per process) are defects.

## Tracker: GitHub Issues

> **One-time bootstrap.** The `status:*` labels below are not GitHub defaults. If they don't exist
> (`gh label list --repo savvagent/otto-flags`), skip the transitions rather than inventing labels.

| Lifecycle step | Command |
|---|---|
| Ref form | `savvagent/otto-flags#123` (`#123` short) |
| Intake / read AC | `gh issue view <n> --repo savvagent/otto-flags --json title,body,labels` |
| Ticketless intake | `gh issue create --repo savvagent/otto-flags --title "<your own sentence>" --body "<the brief verbatim + AC>"` |
| → In Progress / In Review | `gh issue edit <n> --add-label status:in-progress` / `status:in-review` (if the labels exist) |
| Spec / Plan record | committed to `docs/specs/` / `docs/plans/`; referenced from the PR body |
| Close | the merged PR's `Fixes #N` closes it; verify with `gh issue view <n> --json state` |

## Phase 0 — Pre-flight (fresh context)

Intake reads get corrupted by prior conversation cruft — stale paths, abandoned plans.

**Two valid paths to fresh context:**

1. **Subagent dispatch** (default mid-conversation): an `Agent` call with a self-contained prompt —
   issue or brief + "follow otto-flags-development end-to-end" + caller constraints. **See
   "Adaptation: when this skill runs inside a subagent" below.**
2. **`/clear` + re-invoke** in the main thread. **Preferred when review quality matters most** —
   interior `Agent` dispatches work as designed only in the main thread.

**Branch + worktree safety:**

1. `git branch --show-current` in the current working directory.
2. **Trunk-sync check (mandatory):**
   ```bash
   git fetch origin
   git rev-list origin/main..main        # MUST be empty
   ```
   Commits here are unpushed local work that would contaminate the new branch. Surface them
   (commits + file paths); do NOT discard.
3. Create the worktree from `origin/main` explicitly:
   ```bash
   git worktree add .claude/worktrees/<dir> -b <branch> origin/main
   ```
4. Confirm the local Postgres is up and has the role:
   ```bash
   podman ps --format '{{.Names}} {{.Ports}}' | grep 15434   # else: podman compose up -d (main checkout)
   psql postgres://flags:flags@localhost:15434/otto_flags -tc "select 1 from pg_roles where rolname='otto_app'"
   ```
   If the role query prints nothing: `psql … -c "CREATE ROLE otto_app NOLOGIN;"`.
5. If already on a feature branch in a worktree → proceed there.
6. `git status --porcelain` must be clean in the worktree before any edit. Surface unexpected
   changes; do NOT discard them.

Create TodoWrite todos for each phase (1–6) and check them off as you go.

## Adaptation: when this skill runs inside a subagent

A subagent cannot dispatch further subagents (no `Agent` tool). This affects every interior
dispatch (Phase 1 step 4, Phase 2 step 6, Phase 3 steps A/C/E/H, Phase 4 steps 8–9).

- **Interior reviews run as named inline passes** — spec critique, plan critique, implementer
  report, spec-compliance report, code-quality report — each written to the same template in
  `agent-prompts.md`. Only the dispatch mechanism changes.
- **Implementer dispatch collapses to direct execution** under the AUTONOMOUS MODE block.
- **The mandatory review trio and pr-review-toolkit agents are deferred to the parent.** List them
  as `Reviewers to dispatch from parent:` in the final report and **do not merge** — or declare the
  PR mergeable — until the parent confirms they cleared. Rules 4–5 admit no exception here.
- **Review-response (Phase 4 step 9)** runs inline unless the parent set a halt point at step 8.

Inline review by the orchestrator that did the work loses fresh-context isolation. Acceptable, not
equivalent — prefer Phase 0 path 2 for the highest-quality reviews.

## Phase 1 — Intake + spec

> **Fast-path note:** Steps 1 + 2 always run. Steps 3 + 4 are skipped on a qualifying fast-path;
> jump to Phase 2 step 5 and write the minimal plan.

### Step 1: Read the source directly

- **Issue exists:** `gh issue view <n> --repo savvagent/otto-flags --json title,body,labels` — the
  body is the AC.
- **Ticketless:** capture the instruction verbatim, then open an issue from it (title is your own
  sentence; body is the brief verbatim plus the AC you derived). That issue is now the source.

If a teammate summarized it, still read the source — summaries lose AC.

### Step 2: Mark In Progress

`gh issue edit <n> --repo savvagent/otto-flags --add-label status:in-progress` (if the label exists).
**Capture `T_impl_start = now`** (ISO-8601 with offset) for the Phase 6 timeline.

### Step 3: Spec draft (committed, no human review)

Create `docs/specs/YYYY-MM-DD-<slug>-design.md`. Read the existing design spec first for tone and
depth. Structure:

- Title: `# <Change> — Design`, and an `**Issue:** [#N](https://github.com/savvagent/otto-flags/issues/N)` line
- **§1 Brief** — the issue body quoted verbatim, then **Acceptance criteria (verbatim)**
- **Premise corrections** — where the brief's premises do not survive contact with the code
- **Scope** with **In:** and **Out:**
- Numbered sections per component: shape, data model + migration, isolation impact, MCP/SDK
  surface impact, metering impact, testing. Cite `file:line` for code the design touches

Required wherever they fit: **Assumptions** (each with a one-line rationale — the highest-value
section), **Goal & Success Criteria** (one paragraph + 3–5 measurable bullets), **Error Handling &
Edge Cases**, **Risks & Open Questions**. A schema change requires a **Migration** section naming
the new file (`crates/flags-core/migrations/NNNN_<name>.sql`), the exact DDL, the RLS policy and
`otto_app` grants, and the composite foreign keys. A surface change requires an **Out-of-band**
section (platform scope registration, Fly secret/env, SDK packages and changesets).

**Commit the spec draft** before critique. The critique loop revises the committed file.

### Step 4: Spec critique subagent

Dispatch the **`Spec Critique`** template from [`agent-prompts.md`](agent-prompts.md) — **read that
file now and paste the template verbatim.** `subagent_type: general-purpose`. Paste the
Load-Bearing Invariants and the relevant conventions into its Repo Profile.

**Maximum 2 revision rounds (3 reviewer dispatches).** Revise the committed file and redispatch with
the updated text. Remaining issues go into `Risks & Open Questions`; do not loop further. Commit the
approved version once.

## Phase 2 — Plan

### Step 5: Plan draft (committed, no human review)

Create `docs/plans/YYYY-MM-DD-<slug>.md`:

- Title: `# <Change> — Implementation Plan`
- **Goal**, **Architecture**, **Tech Stack** (Rust stable, axum 0.8, sqlx 0.8 runtime queries on
  Postgres 16, rmcp 2.0, tokio; `otto-tenant`/`otto-resource` at the pinned rev; TS SDKs on pnpm 9
  if touched)
- **Spec:** and **Issue:** lines
- **Global Constraints** — at minimum: the three server gates with the explicit `DATABASE_URL`;
  append-only migrations; no SQL outside `flags-core` and no `query!` macros; `begin_live` on
  request paths; client keys never see rules; conventional commits ending `Fixes #N`; **no
  attribution of any kind**
- **File Structure** table — `File | Responsibility`, rows prefixed **Create.**/**Modify.**
- **Task Order & Rationale**
- One `### Task N: <name>` per task with **Files:**, **Interfaces:** (consumes/produces), and `- [ ]`
  steps in **failing-test-first order**: write failing test → run → implement → run → commit, with
  exact paths and the exact command per step

Every task MUST include:
- Exact paths in this layout (`crates/flags-core/src/`, `crates/flags-core/migrations/`,
  `crates/flags-core/tests/db.rs`, `crates/flags-mcp/src/tools/`, `crates/flags-api/src/`,
  `crates/flags-server/src/`, `crates/flags-server/tests/e2e.rs`, `packages/<pkg>/`)
- **A migration step when the schema changes:** a new numbered file (never an edit), RLS + grants +
  composite FKs, and a run of the isolation test
- **Test placement:** domain and SQL behavior in `crates/flags-core/tests/db.rs`
  (`#[sqlx::test(migrator = "flags_core::MIGRATOR")]`); pure logic as unit tests next to the code;
  anything crossing MCP, the SDK API, or webhooks in `crates/flags-server/tests/e2e.rs` against its
  mock platform
- **Reminders for out-of-band artifacts** the task touches (Phase 5 step 14)
- A final commit step with a conventional subject and a why-body

**Commit the plan draft** before critique.

### Step 6: Plan critique subagent

Dispatch the **`Plan Critique`** template from [`agent-prompts.md`](agent-prompts.md), verbatim.
`subagent_type: general-purpose`. Same loop shape as step 4, **maximum 2 revision rounds**;
unresolved issues go into a `## Known Plan Gaps` section. Commit the approved version once.

## Phase 3 — Implement

Read the plan ONCE and extract every task's full text into working memory; one TodoWrite entry per
task. **Implementers never re-read the plan** — they get the task text inline.

**Sequential, not parallel.** Implementers on the same branch conflict on the working tree.

For each task in plan order:

### A. Dispatch implementer

**`Implementer Dispatch`** template from [`agent-prompts.md`](agent-prompts.md), verbatim (the
`## AUTONOMOUS MODE` block and `## Report Format` are load-bearing). `subagent_type:
general-purpose` — or `rust-pro` for a task confined to `crates/`, `typescript-pro` for one confined
to `packages/`. Model: `haiku` for mechanical 1–2-file tasks; inherit for multi-file work; `opus`
for tasks the plan flags as design judgment.

### B. Handle implementer status

| Status | Action |
|---|---|
| `DONE` | Spec compliance review (C) |
| `DONE_WITH_CONCERNS` | Correctness/scope concern → fix dispatch now. Minor → ledger, proceed |
| `NEEDS_CONTEXT` | Discoverable → re-dispatch with it. Unknowable → BLOCKED |
| `BLOCKED` | Stop & Escalate |

**Verify the evidence, not the claim.** A report without the three gate outputs (or with clippy
warnings waved through) is no evidence; re-dispatch.

### C. Spec compliance review

**`Spec Compliance Review`** template, verbatim (`## CRITICAL: Do Not Trust The Report` is
load-bearing). `subagent_type: general-purpose`.

### D. Spec fix loop (max 2 fix dispatches)

✅ → quality review. ❌ → re-dispatch with status `FIX_SPEC_ISSUES` and the findings. Three failed
spec reviews in a row → escalate.

### E. Code quality review

`BASE_SHA = git rev-parse HEAD~<N>` (N = commits this task produced), `HEAD_SHA = git rev-parse
HEAD`. **`Code Quality Review`** template, verbatim. `subagent_type: code-reviewer`.

### F. Quality fix loop (max 2 fix dispatches)

No Critical/Important → task complete; Minor issues to the ledger. Critical/Important → fix
dispatch, re-review. Three failures in a row → escalate. "Approved with suggestions" is DONE.

### G. Per-task ledger

Per task: name, final status, assumptions, concerns, Minor issues left. Feeds Phase 6.

### H. Final code review (after all tasks)

> Skip when N=1 (fast-path) AND step E reported no Critical/Important.

**`Final Code Review`** template, verbatim. `subagent_type: code-reviewer`. One fix round, then
escalate if still failing.

**Before opening the PR, run every gate CI runs, from the worktree:**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace
git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/'   # MUST be empty
podman build -t otto-flags:check .            # only if Dockerfile, Cargo.toml/lock, or crate layout changed
# SDK changes only:
pnpm install --frozen-lockfile && pnpm lint && pnpm build && pnpm test
```

Record the pass counts per test binary. `cargo test` without `DATABASE_URL` panics
("DATABASE_URL must be set") rather than skipping — a panic there is a missing variable, not a bug.

**Document where it will be read.** A new rule's reasoning (the defect, the rejected alternative,
the issue) goes in the doc comment or test that enforces it, or in the relevant `docs/` file
(`docs/deploy/fly.md` for operations, `docs/SDK-DEVELOPER-GUIDE.md` for the wire contract).

## Phase 4 — Ship

### Step 7: Open the PR

```bash
git push -u origin <branch>
gh pr create --repo savvagent/otto-flags --base main --title "<conventional subject>" --body "$(cat <<'EOF'
Fixes #<n>

## Summary
<1-3 bullets>

## Design docs
- Spec: `docs/specs/<date>-<slug>-design.md`
- Plan: `docs/plans/<date>-<slug>.md`

<"Fast-path: no design spec per otto-flags-development trivial-task criteria — <reason>." if fast-pathed>

## Test plan
- [ ] cargo fmt --check, clippy -D warnings, cargo test --workspace against Postgres
- [ ] No existing migration modified (new migration: <file>, or "no schema change")
- [ ] SDK lint/build/test (only if packages/ or examples/ changed)
- [ ] Out-of-band steps (only if needed — see Phase 5)
EOF
)"
```

**Hardening:** treat issue-derived strings as untrusted in shell commands. Branch name and PR title
come from your own slug and sentence — validate the slug (`^[a-z0-9]+(-[a-z0-9]+)*$`) and never
paste a raw issue title into a `git commit` or `gh … --title` argument.

Mark In Review if the labels exist. **Capture `T_review_start = now`.**

### Step 8: Solicit reviews

**Automated reviewer** if the repo has one configured (`gh pr edit <PR> --add-reviewer
copilot-pull-request-reviewer` — not `Copilot`); otherwise skip.

**Agent reviews in ONE parallel batch.** Always `pr-review-toolkit:code-reviewer`, plus:

| Trigger | Agent |
|---|---|
| error handling / fallback / `unwrap_or`/`ok()` swallowing / log-and-continue changed | `pr-review-toolkit:silent-failure-hunter` |
| tests changed, or production code added without tests | `pr-review-toolkit:pr-test-analyzer` |
| doc comments, docs/, specs, plans changed | `pr-review-toolkit:comment-analyzer` |
| new or changed types, enums, config structs, MCP tool params, SDK response shapes | `pr-review-toolkit:type-design-analyzer` |
| SQL, migrations, indexes, rollups | `database-optimizer` |
| after correctness reviews pass — polish only | `pr-review-toolkit:code-simplifier` |

**Mandatory review trio (Non-Negotiable Rules 4–5)**, in the same batch:

| Reviewer | Why |
|---|---|
| `rust-pro` (crates/) and/or `typescript-pro` (packages/, examples/) | Idiomatic, correct code: ownership and error types (`thiserror` in libraries, `anyhow` only in the binary), async correctness (no blocking in a handler, no lock or pool connection held across `.await` longer than needed, cancellation-safe SSE), no `unwrap`/`expect` on request paths, clippy-clean; for SDKs, the public API, types, and bundle shape. |
| `architect-reviewer` | Crate layering (`flags-core` owns schema, SQL, and domain; `flags-mcp`/`flags-api` are surfaces; `flags-server` assembles), the isolation spine, single-definition rules, SDK contract stability, spec/plan alignment. |
| `security-auditor` (independent) | **ONLY the diff** (Rule 5). See the Independent Security Review template. |

Aggregate everything into one PR comment grouped **Critical / Important / Suggestions / Strengths**.

### Step 9: Review-response subagent

Once reviews post, dispatch the **`Review-Response Subagent`** template, verbatim (fix-or-dismiss,
thread-resolve mutation, and inline replies are load-bearing). `subagent_type: general-purpose`.

### Step 10: PR review loop

Each new review round gets a NEW review-response subagent. **Idempotent:** first enumerate
unresolved threads; if none are new, exit with no commits or replies.

```bash
gh pr view <PR> --repo savvagent/otto-flags --json reviewDecision,reviews,statusCheckRollup
gh pr checks <PR> --repo savvagent/otto-flags
gh api repos/savvagent/otto-flags/pulls/<PR>/comments
```

| State | Action |
|---|---|
| All threads resolved + checks green on the head commit (`rust`, `docker-build`, `migrations-append-only`, and `Test` if SDKs changed) | Merge (step 11) |
| Checks pending | `gh pr checks <PR> --watch` |
| Open threads you cannot address (security / missing requirement / ambiguous) | Escalate |
| Same thread unresolved after you replied, across iterations | Wake the human. Do not retry silently |
| A check failed | Treat as a "fix this" comment |

### Step 11: Merge

**Merging deploys.** Before merging, confirm the change is deploy-safe *as merged*: a migration
must be compatible with the code still running during the rollout (additive first; drop columns
in a later release), a new required config key or secret must already be set on Fly (handed to Rob
**before** the merge — Phase 5 step 14), and a new scope must already be registered at the
platform.

The merge queue cannot enqueue and auto-merge is disabled; Rob is an org admin and has authorized
merging with his bypass. Run from the **main checkout**, not the worktree:

```bash
cd <main-checkout-root>
gh pr merge <PR> --repo savvagent/otto-flags --admin --squash
git push origin --delete <branch>           # --delete-branch is rejected while a queue is configured
```

Only merge once the checks are green and every review cleared — the bypass skips GitHub's gates,
so the gates are on you. **Capture `T_pipeline_start = now`.**

### Step 12: Clean up + record-as-shipped

```bash
cd <main-checkout-root>
git switch main && git pull --ff-only
git worktree remove .claude/worktrees/<dir>
git branch -D <branch>          # squash-merged, so -d will not recognise it as merged
```

**Record-as-shipped (mandatory):** in a fresh worktree + PR (never a direct commit), set the spec's
status line to `IMPLEMENTED (#<PR>)` and tick the plan's tasks. Rationale that surfaced in review or
deploy goes into the spec or the enforcing doc comment.

## Phase 5 — Deploy, verify, close

### Step 13: Confirm the deploy took

The merge triggers CI on `main`; its `deploy` job ships to Fly. Watch it:

```bash
gh run list --repo savvagent/otto-flags --branch main --workflow CI --limit 1
gh run watch <run-id> --repo savvagent/otto-flags --exit-status
fly releases -a otto-flags | head -5        # the new release is on top
fly status -a otto-flags
curl -s https://otto-flags.savvagent.com/readyz     # the first request may resume a suspended machine
fly logs -a otto-flags --no-tail | tail -50
```

In the logs, a healthy boot shows `applying migrations`, the isolation summary naming
`otto_app` (the server refuses to serve if isolation is not enforced), and `the otto platform
accepted this service's credential`. A machine that fails the
`/readyz` check rolls back; then the change is **not live** — say so, do not report "shipped".

**Capture `T_verify_start = now`.**

### Step 14: Out-of-band artifact verification

Nothing in CI applies these. Check whatever the change touched; a vacuous item ("no migration") is
satisfied — say so explicitly.

- **Migration** — runs at boot under an advisory lock. Confirm it in the boot logs. A failure
  crash-loops the new machine and the deploy rolls back.
- **New config key / secret** — non-secret values go in `fly.toml` `[env]` (in the PR). Secrets need
  `fly secrets set -a otto-flags --stage NAME=…` **before** the merge; hand that to Rob with the
  exact name. Update `.env.example` and the secrets table in `docs/deploy/fly.md`.
- **New scope** — update `flags_core::scopes::KNOWN`/`DEFAULT` and hand Rob the platform-side
  `otto-platform-server resource register …` re-registration from `docs/deploy/fly.md` **before**
  the merge, or tokens carrying the scope are never issued.
- **otto-platform pin bump** — confirm both crates moved to the same rev, and that the platform's
  introspection and webhook payloads at that rev match what is deployed there.
- **SDK packages** — a changeset is in the PR; `release.yml` does not publish (it targets
  `master`), so state that the package is **merged but not published**.
- **Deploy shape** (`fly.toml`, `Dockerfile`, workflows) — the image built in CI, the release is
  healthy, and nothing added recurring cost (Rule 8).

### Step 15: Production verification

**Read-only and narrowly targeted (Rule 7).**

- `/readyz` and `/.well-known/oauth-protected-resource` respond.
- For an MCP change: exercise the tool through an MCP client pointed at
  `https://otto-flags.savvagent.com/mcp` (`whoami` first), or confirm the route answers.
- For an SDK API change: call the changed route with a test app's key, if one exists.
- For a data question: SELECT-only, e.g. the usage backlog
  (`SELECT count(*), max(attempts), max(last_error) FROM usage_outbox;` — drains to 0 within
  seconds). Any **write** is handed to Rob with the exact statement and database named.

### Step 16: Close

The PR's `Fixes #N` closes the issue; verify with `gh issue view <n> --repo savvagent/otto-flags
--json state` and close it by hand with a summary comment if not.

```
Shipped.

PR: <url>
Deploy: <fly release version / "not live — see below">
Out-of-band applied: <migration/secret/scope/pin/changeset, or "none">
Smoke: <one-line outcome>
```

## Phase 6 — Final summary

```
otto-flags-development complete.

Source: #<n> — <title> — Closed
PR: <url>
Branch: <branch> (deleted, worktree removed)
Spec: docs/specs/<date>-<slug>-design.md
Plan: docs/plans/<date>-<slug>.md
Tasks completed: N / N
Commits: <count>
Tests: <per-binary pass counts; fmt + clippy clean>
Deploy: <fly release version>
Timeline: <T_impl_start → T_review_start → T_pipeline_start → T_verify_start>

Out-of-band applied: <list, or "none">

Assumptions worth reviewing (from spec + per-task ledger):
- <up to 5>

Minor issues left unaddressed (intentional, low-priority):
- <bullet, or "none">

Final reviewer assessment: <Ready / Needs follow-up — details>
```

Then STOP. Do not pick up the next task.

## Stop & Escalate

Stop and return control when ANY of these is true:

1. A task is BLOCKED and one re-dispatch with more context did not unblock it.
2. A task fails spec review, or quality review with Critical/Important issues, three times in a row.
3. Test infrastructure is broken — the local Postgres will not start, or the suite cannot run.
4. The plan is internally inconsistent, or the AC contradicts the spec/plan mid-flight.
5. The pipeline has run unreasonably long without progress.
6. A security finding (auth, isolation, injection, secrets, key exposure) from any review.
7. A change would weaken the isolation spine: SQL outside `flags-core`, a request path on
   `Db::begin`, a tenant table without RLS or composite FKs, rules visible to a client key, a
   platform call on the SDK path.
8. A breaking change to the SDK REST contract or to evaluation bucketing.
9. An existing migration would need to change, or a migration is not deploy-compatible with the
   running code.
10. The deploy failed, rolled back, or did not run after a merge.
11. A production write, secret, or platform registration is required — hand it to Rob.
12. The change would add recurring cost (Rule 8).
13. The same bug pattern exists elsewhere — file a follow-up issue; do NOT silently widen scope.

On escalation:

```
otto-flags-development halted at Phase <N> — <step name>.

Reason: <condition, with specifics>
Source: #<n>
Branch: <branch>
Worktree: .claude/worktrees/<dir>
PR: <url, if open>
Last successful step: <step>
Commits so far: <git log --oneline origin/main..HEAD>
Recommended next step: <suggestion>
```

Then STOP. Do not push, open a PR, merge, or close.

## Calibration vs. Skipping

Calibrate effort to risk within a step; never eliminate a step.

| Step | Cheapest valid form | Skip? |
|---|---|---|
| Read source / open issue | 20-second read of body + AC | NEVER |
| Spec draft + critique | 1-page spec, 1 reviewer dispatch | Only on a qualifying fast-path |
| Plan draft | Minimal single-task plan (committed) | NEVER |
| Plan critique | 1 reviewer dispatch | Only on a qualifying fast-path |
| Worktree off `origin/main` | `git worktree add …` | NEVER |
| Implementer + spec compliance + quality review | 1 dispatch each | NEVER |
| fmt + clippy + `cargo test` with `DATABASE_URL` | 1 run each before the PR | NEVER |
| Migration append-only check | 1 `git diff --diff-filter=MDR` | Vacuous when no migration file changed |
| SDK lint/build/test | 1 run | Only when no SDK/example file changed |
| Language expert + architect + blind security review | 1 parallel dispatch each | NEVER |
| pr-review-toolkit agents | 1 parallel dispatch | NEVER |
| CI green on the head commit before merge | `gh pr checks` | NEVER |
| Deploy confirmation (#13) | `gh run watch` + `fly releases` + `/readyz` | NEVER |
| Out-of-band (#14) / production check (#15) | 30 seconds per touched surface | NEVER (vacuous is fine) |
| Cleanup + record-as-shipped | worktree remove + record PR | NEVER |

## Common Rationalizations (All Are Violations)

| Excuse | Reality |
|---|---|
| "It's a small change, skip the spec/plan" | Only the spec, and only if every fast-path criterion holds. The plan is never skipped. |
| "No issue exists, so no issue reference" | Open one at intake. Every PR references its issue. |
| "`cargo test` passed" | CI also runs fmt and clippy `-D warnings`, checks migrations, and builds the image. |
| "I'll fix the typo in the old migration" | sqlx checksums it; production refuses to boot. New file only. |
| "I'll use `query!` for compile-time checking" | The image builds without a database. Runtime `sqlx::query` only. |
| "This query is simpler in flags-api" | No SQL outside `flags-core`. Add a method on the extension trait. |
| "`Db::begin` is fine here" | Request paths use `begin_live` so deleted orgs and removed members are refused. |
| "The FK on `id` is enough" | FK checks bypass RLS. Composite `(org_id, id)`. |
| "Client keys can see the rules, it's convenient for the SDK" | Client keys are public. Never. |
| "I'll check the member at the platform from the SDK path" | The SDK path never calls the platform. |
| "Renaming this response field is cleaner" | Published SDKs depend on it. Additive only. |
| "Merged, so it's shipped" | Merged starts a deploy. Shipped is a healthy release in `fly releases` and `/readyz`. |
| "I'll set the secret myself, it's faster" | Secrets, production writes, and platform registration go to Rob. |
| "I'll run the tests against the Fly database" | `#[sqlx::test]` creates and drops databases on a shared superuser instance. Never. |
| "I'll bump the VM / keep a machine running" | Recurring cost needs Rob's say-so. |
| "The trio is overkill for this PR" | Every PR gets the language expert, the architect, and the blind security review. |
| "I'll give the security agent the spec too" | It receives ONLY the diff. |
| "Copilot's comments are noise" | Read each; fix or dismiss with reasoning, then resolve the thread. |
| "Parallel implementers are faster" | Same branch = conflicts. |
| "Quality reviewer flagged Minor issues, must loop" | Minor ≠ blocker. |
| "I'll add attribution to the commit" | Never. |

## Red Flags — STOP

- About to code in the main checkout instead of a worktree
- About to modify, rename, or delete a file under `crates/flags-core/migrations/`
- About to write SQL outside `flags-core`, or a `query!`/`query_as!` macro
- About to open a request-path transaction with `Db::begin`
- About to add a tenant table without RLS, `otto_app` grants, and composite FKs
- About to return targeting rules, or another org's data, to a client key
- About to call the platform from the SDK path
- About to change an SDK route, field, or status code incompatibly
- About to change rollout bucketing
- About to point `DATABASE_URL` at anything but local Postgres
- About to report green without fmt and clippy `-D warnings`
- About to open a PR with no `Fixes #N`
- About to open or merge without the trio and the toolkit reviews
- About to hand the security agent anything but the diff
- About to merge with red or pending checks or unresolved threads
- About to merge a migration that is unsafe while the old code is still running
- About to merge a change that needs a secret or scope registration Rob hasn't applied yet
- About to run `fly secrets set`, a production write, or a platform `resource` command yourself
- About to add recurring cost
- About to declare "shipped" without a healthy release and `/readyz`
- About to skip record-as-shipped
- About to invoke `AskUserQuestion` mid-pipeline
- "It's just a one-line change" / "I already tested it manually" / "I'll add AI attribution"

Each thought = stop, do the step (calibrated), then continue.

## Cross-references

- [`agent-prompts.md`](agent-prompts.md) — verbatim dispatch templates (read at each dispatch step)
- `general-development` — the repo-agnostic sibling
- `README.md` — overview, local development, license split
- `VISION.md` — product direction (agents manage flags; no console in v1)
- `docs/specs/2026-09-15-otto-flags-design.md` — the product design
- `docs/plans/2026-10-09-build-and-deploy.md` — the v1 decisions, MCP tool table, deferred work
- `docs/deploy/fly.md` — topology, secrets, platform registration, deploy and verification
- `docs/SDK-DEVELOPER-GUIDE.md` — the SDK wire contract
- `crates/*/src/lib.rs` module docs — each crate's responsibilities and rules
