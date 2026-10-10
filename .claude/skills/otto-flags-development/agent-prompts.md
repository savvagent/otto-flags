# otto-flags-development — Agent Dispatch Prompt Templates

Verbatim prompt bodies for every `Agent`-tool dispatch in `otto-flags-development`. SKILL.md owns
the *decision* logic (when to dispatch, which `model:`/`subagent_type`, status handling, fix-loop
caps); this file owns the prompt *text* you paste.

**Use the template exactly. Do not improvise a dispatch prompt body.** Fill the `<...>`
placeholders from working memory (spec / plan / task text / report verbatim, plus the "Repository
Conventions" and "Load-Bearing Invariants" sections of SKILL.md). Honor the `subagent_type` named
in each heading.

`<ref>` is the GitHub issue (`savvagent/otto-flags#123`). Every run has one — ticketless work opens
an issue at intake.

The spec and plan are committed repo files (`docs/specs/`, `docs/plans/`). Paste the full text
inline anyway, and give the path so the reviewer can check the committed version.

**The gates are never a bare `cargo test`.** Every template that runs checks carries all three:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace
```

plus `pnpm lint && pnpm build && pnpm test` when `packages/` or `examples/` changed.

---

## Spec Critique — Phase 1 Step 4 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Review spec document"
  prompt: |
    You are a spec document reviewer. Verify this spec is complete and ready for planning.

    Spec to review (full text inline; the committed copy is at <docs/specs/<date>-<slug>-design.md>):
    <PASTE FULL SPEC TEXT>

    Repo context to check against (from otto-flags-development's Load-Bearing Invariants):
    <PASTE THE LOAD-BEARING INVARIANTS SECTION — RLS under otto_app proven by
     verify_tenant_isolation, begin_live on request paths, no SQL outside flags-core and no
     query! macros, composite (org_id, id) foreign keys, hashed SDK keys and client keys never
     seeing rules, no platform call on the SDK path, the stable SDK REST contract, MCP scopes and
     the fresh owner/admin lookup, metering in the tool's own transaction, versioned flags with
     pg_notify in the same transaction, counted-not-stored evaluations, untrusted SDK text,
     deterministic rollout bucketing, fail-closed platform webhooks, the small shared database —
     plus append-only migrations and the out-of-band surfaces (Fly secrets, platform scope
     registration, the otto-platform pin, SDK changesets)>

    Check:
    - Completeness: TODOs, placeholders, "TBD", incomplete sections
    - Consistency: internal contradictions, conflicting requirements
    - Clarity: requirements ambiguous enough to cause someone to build the wrong thing
    - Scope: focused enough for a single plan; respects the crate layering
      (flags-core owns schema, SQL, and domain; flags-mcp and flags-api are surfaces;
      flags-server assembles)
    - YAGNI: unrequested features, over-engineering
    - Alignment with the source AC (cite ref <ref>)
    - Whether any design choice weakens the isolation spine: a tenant table without RLS or
      composite FKs, SQL outside flags-core, a request path on Db::begin, rules exposed to a
      client key, a platform call on the SDK path, an incompatible SDK contract change, a change
      to rollout bucketing, raw evaluation context persisted
    - If the schema changes: does the spec name a NEW migration file (never an edit to an
      existing one), the exact DDL, RLS + otto_app grants, and the composite FKs, and is the
      migration compatible with the code still running during a rollout?
    - If a surface changes: does it list the out-of-band steps (Fly secret/env, platform scope
      registration, SDK packages + changesets) and any recurring-cost impact?

    Only flag issues that would cause real problems during planning. Approve unless there are serious gaps.

    Output:
    ## Spec Review
    Status: Approved | Issues Found
    Issues: - [Section X]: [issue] - [why it matters]
    Recommendations (advisory): - [...]
```

---

## Plan Critique — Phase 2 Step 6 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Review plan document"
  prompt: |
    You are a plan document reviewer. Verify this plan is complete and ready for implementation.

    Plan to review (full text inline; the committed copy is at <docs/plans/<date>-<slug>.md>):
    <PASTE FULL PLAN TEXT>

    Spec for reference (full text inline; the committed copy is at <docs/specs/<date>-<slug>-design.md>):
    <PASTE FULL SPEC TEXT>

    Repo requirements to check against (from otto-flags-development):
    <PASTE THE LOAD-BEARING INVARIANTS + Repository Conventions — the three server gates with an
     explicit DATABASE_URL, the SDK gates, append-only migrations, conventional commits ending
     "Fixes #N", the out-of-band deploy surfaces, the "no AI attribution" rule>

    Check completeness, spec alignment, task decomposition, buildability, and whether each task:
    - uses exact paths in this repo's layout (crates/flags-core/src/, crates/flags-core/migrations/,
      crates/flags-core/tests/db.rs, crates/flags-mcp/src/tools/, crates/flags-api/src/,
      crates/flags-server/src/, crates/flags-server/tests/e2e.rs, packages/<pkg>/)
    - orders steps failing-test-first (write failing test -> run -> implement -> run -> commit)
    - names the exact command per step, including DATABASE_URL for anything using #[sqlx::test]
    - puts tests where they belong: SQL and domain behavior in crates/flags-core/tests/db.rs with
      #[sqlx::test(migrator = "flags_core::MIGRATOR")]; pure logic as unit tests; MCP, SDK API,
      and webhook behavior in crates/flags-server/tests/e2e.rs against the mock platform
    - adds a schema change as a NEW numbered migration with RLS, otto_app grants, and composite
      FKs, and runs the tenant-isolation test afterwards
    - keeps SQL in flags-core as runtime sqlx::query (no query! macros)
    - flags any isolation-spine, SDK-contract, metering, or bucketing impact
    - flags every out-of-band surface it touches (Fly secret/env, platform scope registration,
      otto-platform pin, SDK changeset) so Phase 5 covers it

    Only flag issues that would cause an implementer to build the wrong thing or get stuck.

    Output:
    ## Plan Review
    Status: Approved | Issues Found
    Issues: - [Task X, Step Y]: [issue] - [why it matters]
    Recommendations (advisory): - [...]
```

---

## Implementer Dispatch — Phase 3 Step A — `subagent_type: general-purpose` (or `rust-pro` / `typescript-pro`)

```
Agent tool:
  subagent_type: general-purpose
  description: "Implement Task N: <name>"
  prompt: |
    You are implementing Task N: <name>

    ## Task Description
    <FULL TEXT of the task pasted inline — do not reference the plan file>

    ## Context
    <2-4 sentences: where this fits, dependencies on prior tasks, architectural notes, the
    Load-Bearing Invariants that apply to THIS task (RLS under otto_app, begin_live, SQL only in
    flags-core as runtime sqlx::query, composite FKs, client keys never see rules, no platform
    call on the SDK path, the stable SDK contract, metering in the tool's own transaction,
    versioning + pg_notify in the same transaction), any out-of-band surface it touches, and the
    "no AI attribution" rule>

    ## AUTONOMOUS MODE — IMPORTANT

    You are running inside an autonomous pipeline. Do NOT ask clarifying questions.
    There is no developer available to answer mid-run.

    Instead:
    - When the task is ambiguous, pick the most reasonable interpretation given the
      surrounding code and the spec. Document the assumption in your report.
    - If the assumption is high-risk (could plausibly be wrong in a way the developer
      would care about), report DONE_WITH_CONCERNS and list the assumption explicitly.
    - Only return BLOCKED if you genuinely cannot proceed without information that
      cannot be reasonably inferred (e.g., a missing credential, an undocumented external
      contract). Do NOT return BLOCKED for stylistic ambiguity.

    ## Your Job
    1. Work ONLY in the worktree at: <worktree path> (.claude/worktrees/<dir>). Never touch the main
       checkout. Never commit or push to main.
    2. Follow the task's TDD steps in order: failing test -> run -> implement -> run -> commit.
    3. Before reporting, run all three server gates and report each outcome:
         cargo fmt --all -- --check
         cargo clippy --workspace --all-targets -- -D warnings
         DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace
       (plus `pnpm lint && pnpm build && pnpm test` if you touched packages/ or examples/).
       Run `cargo fmt --all` to fix formatting; never silence a clippy lint with `#[allow]` unless
       the task says so. DATABASE_URL must point at the local Postgres on port 15434 — never at
       any Fly database.
    4. Never modify, rename, or delete an existing file in crates/flags-core/migrations/. A schema
       change is a new, next-numbered file with RLS enabled, grants to otto_app, and composite
       (org_id, id) foreign keys like the baseline.
    5. SQL lives only in flags-core, as runtime sqlx::query / query_as calls (never query! macros —
       the Docker build has no database), exposed as methods on the extension traits over
       otto_tenant::Tx. Request paths open transactions with platform_events::begin_live.
    6. A new dependency goes in [workspace.dependencies] in the root Cargo.toml and is committed
       with Cargo.lock. Never bump the otto-platform rev unless the task says to (both crates
       together).
    7. Use exact file paths and commands from the task. Do not invent your own.
    8. Self-review before reporting (completeness, quality, YAGNI, testing).
    9. Commit per the task's steps with a conventional subject (`fix: …`, `feat: …`, `docs: …`,
       lower-case after the prefix, imperative) and a body explaining why, ending `Fixes #<n>`.
       Never add AI/Co-Authored-By attribution.
    10. Put the reasoning for a new rule in the doc comment or test that enforces it.

    ## Report Format
    - Status: DONE | DONE_WITH_CONCERNS | BLOCKED | NEEDS_CONTEXT
    - Files changed (with commit SHAs)
    - Gates: fmt / clippy / test outcomes (the exact commands + pass counts per test binary)
    - Migration: new file name, or "no schema change"
    - Assumptions made (with one-line rationale each)
    - Concerns or blockers (if any)
```

---

## Spec Compliance Review — Phase 3 Step C — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Spec compliance: Task N"
  prompt: |
    You are reviewing whether an implementation matches its specification.

    ## What Was Requested
    <FULL TEXT of the task — same as implementer received>

    ## What Implementer Claims They Built
    <implementer's report verbatim>

    ## CRITICAL: Do Not Trust The Report
    Read the actual code at the commit SHAs they listed. Verify line-by-line.

    Check:
    - Missing requirements (claimed implemented but actually skipped)
    - Extra work (built features not requested)
    - Misinterpretations (right feature, wrong way)
    - Gate evidence: fmt, clippy -D warnings, and cargo test with DATABASE_URL all reported
    - otto-flags gotchas:
      - no existing migration modified; a new one has RLS, otto_app grants, composite FKs
      - no SQL outside flags-core; no query! macros
      - request paths use begin_live, not Db::begin
      - nothing returned to a client (sdk_) key exposes targeting rules or other orgs' data
      - no platform call on the SDK path
      - SDK routes, fields, and status codes changed only additively
      - MCP tools check their scope; app/key management keeps the fresh owner/admin lookup
      - billable MCP writes record usage in the tool's own transaction
      - flag changes write a flag_versions snapshot and pg_notify in the same transaction
      - telemetry counted into rollups, never on the evaluate endpoint; no raw context stored
      - tests in the right place (db.rs with #[sqlx::test], unit tests, or e2e.rs)
      - commits are conventional, end "Fixes #<n>", and carry no AI attribution
      - out-of-band surfaces the task touched are flagged for Phase 5

    Report:
    - ✅ Spec compliant
    - ❌ Issues found: [list with file:line refs]
```

---

## Code Quality Review — Phase 3 Step E — `subagent_type: code-reviewer`

Capture `BASE_SHA = git rev-parse HEAD~<N>` (N = commits this task produced) and
`HEAD_SHA = git rev-parse HEAD` first.

```
Agent tool:
  subagent_type: code-reviewer
  description: "Quality: Task N"
  prompt: |
    Review the code changes between <BASE_SHA> and <HEAD_SHA>.

    Plan/requirements: Task N (full text inline; the plan is committed at
    docs/plans/<date>-<slug>.md):
    <FULL TEXT of task>

    Check standard code-quality concerns plus:
    - One clear responsibility per module; units decomposed for independent testing
    - Following the plan's file structure; no files grown far beyond the task
    - otto-flags conventions (README, docs/deploy/fly.md, each crate's lib.rs module docs, and
      otto-flags-development's Load-Bearing Invariants):
      - crate layering: flags-core owns schema, SQL, and domain; flags-mcp and flags-api are thin
        surfaces; flags-server assembles config, routing, and background tasks
      - errors: thiserror types in the libraries, anyhow only in the binary; no unwrap/expect on a
        request path; database error text never reaches an MCP response
      - async: no blocking work in a handler, no pool connection held across a long await, SSE
        streams do not hold a pool connection each
      - queries bounded and indexed; no per-row fan-out on a 5-connection pool
      - doc comments explain why, matching the surrounding density
      - no AI attribution in comments or docs

    Report: Strengths, Issues (Critical / Important / Minor), Assessment.
```

---

## Final Code Review — Phase 3 Step H — `subagent_type: code-reviewer`

```
Agent tool:
  subagent_type: code-reviewer
  description: "Final review: <slug>"
  prompt: |
    Final review of the complete implementation.

    Plan (full text inline; committed at docs/plans/<date>-<slug>.md):
    <PASTE FULL PLAN TEXT>
    Spec (full text inline; committed at docs/specs/<date>-<slug>-design.md):
    <PASTE FULL SPEC TEXT>
    Branch: <branch-name>
    Diff range: <merge-base-with-origin/main>..HEAD

    Verify:
    - All plan tasks are implemented end-to-end
    - The implementation achieves the spec's success criteria
    - No dead code, leftover debug, ignored tests, or #[allow] added to quiet clippy
    - Test coverage fits what was built, including tenant isolation for any new table and e2e
      coverage for any new MCP tool or SDK route
    - Convention compliance: append-only migrations, the isolation spine, the stable SDK
      contract, metering and versioning rules, Cargo.lock committed with dependency changes,
      changesets for published-package changes, conventional commits ending "Fixes #<n>" with no
      attribution, out-of-band surfaces flagged for Phase 5

    Report: Strengths, Issues, Overall assessment (Ready to merge / Needs work).
```

---

## Mandatory Review Trio — Phase 4 Step 8

Every PR gets all three, no exceptions (Non-Negotiable Rules 4–5), in one parallel batch, each
reading the actual diff. Use `rust-pro` when `crates/` changed and `typescript-pro` when
`packages/` or `examples/` changed — both when both did.

### Language expert — `subagent_type: rust-pro`

```
Agent tool:
  subagent_type: rust-pro
  description: "Rust review: <subject>"
  prompt: |
    You are the mandatory Rust-expert reviewer for PR #<N> on savvagent/otto-flags (<ref>).

    Review the actual diff:
      gh pr diff <N> --repo savvagent/otto-flags
      gh pr view <N> --repo savvagent/otto-flags --json commits,files,title,body

    Check against this repo's Rust conventions:
    - Idiomatic Rust on stable (MSRV 1.88, edition 2021); clippy-clean under -D warnings without
      new #[allow]s
    - Errors: thiserror types in flags-core/flags-mcp/flags-api, anyhow only in flags-server's
      binary; no unwrap/expect/panic on a request path; database error text never surfaces in an
      MCP tool response
    - Async (tokio, axum 0.8): no blocking calls in handlers; no lock or pool connection held
      across an await longer than needed; SSE streams cancellation-safe and not holding a pool
      connection each; background tasks shut down on SIGTERM so the final usage flush runs
    - sqlx 0.8: runtime sqlx::query/query_as only (no query! macros — the image builds without a
      database); all SQL in flags-core; transactions through otto_tenant::Tx; request paths via
      platform_events::begin_live
    - Types: newtype ids from otto_tenant::ids / flags_core::ids rather than bare Uuid; serde
      shapes on the SDK contract unchanged or strictly additive
    - Tests: #[sqlx::test(migrator = "flags_core::MIGRATOR")] for database behavior, unit tests
      for pure logic, crates/flags-server/tests/e2e.rs for cross-surface behavior
    - Dependencies declared in [workspace.dependencies]; Cargo.lock committed
    - No AI attribution in commits, comments, or docs

    Report: Strengths, Issues (Critical / Important / Minor), Assessment (Approve / Request changes).
```

### Language expert — `subagent_type: typescript-pro` (SDK changes)

```
Agent tool:
  subagent_type: typescript-pro
  description: "TypeScript review: <subject>"
  prompt: |
    You are the mandatory TypeScript-expert reviewer for PR #<N> on savvagent/otto-flags (<ref>).

    Review the actual diff (packages/ and examples/ only):
      gh pr diff <N> --repo savvagent/otto-flags
      gh pr view <N> --repo savvagent/otto-flags --json commits,files,title,body

    Check:
    - The package's public API: breaking changes need a major changeset; additive changes a
      minor or patch one; docs/test-only changes need none
    - Types are strict and match the server's SDK contract (docs/SDK-DEVELOPER-GUIDE.md)
    - The SDK still falls back to defaults and cached values when the API is unreachable, and
      never throws into the host application for a flag lookup
    - Client-side code never carries a server (srv_) key
    - The package keeps "license": "MIT" and its LICENSE file
    - lint, build, and test scripts pass; no new tooling added as a side-effect
    - No AI attribution in commits, comments, or docs

    Report: Strengths, Issues (Critical / Important / Minor), Assessment (Approve / Request changes).
```

### Architect — `subagent_type: architect-reviewer`

```
Agent tool:
  subagent_type: architect-reviewer
  description: "Architect review: <subject>"
  prompt: |
    You are the mandatory architectural reviewer for PR #<N> on savvagent/otto-flags (<ref>).

    Review the actual diff and the spec/plan for this change:
      gh pr diff <N> --repo savvagent/otto-flags
      gh pr view <N> --repo savvagent/otto-flags --json commits,files,title,body
      # Spec/plan (committed): docs/specs/<date>-<slug>-design.md, docs/plans/<date>-<slug>.md

    The authorities are docs/specs/2026-09-15-otto-flags-design.md (product design),
    docs/plans/2026-10-09-build-and-deploy.md (v1 decisions), docs/deploy/fly.md (operations),
    docs/SDK-DEVELOPER-GUIDE.md (wire contract), and each crate's lib.rs module docs. otto-flags
    is a resource server of the otto platform: identity, OAuth, orgs, teams, and billing are the
    platform's; this service holds only flags.

    Check:
    - Crate layering: flags-core owns schema, SQL, domain, and the pure evaluation engine;
      flags-mcp and flags-api are surfaces over it; flags-server assembles. No SQL outside
      flags-core
    - Tenant isolation: tenant queries are extension-trait methods on otto_tenant::Tx; request
      paths use begin_live; new tenant tables enable RLS, grant to otto_app, and use composite
      (org_id, id) FKs; deliberate pre-org lookup tables stay minimal
    - Boundaries: the SDK path never calls the platform; client keys never see rules; the SDK
      REST contract changes only additively; MCP tools check scopes and app/key management keeps
      the fresh owner/admin lookup
    - Single definitions: metering in the tool's own transaction via usage_outbox; flag versions
      and pg_notify in the same transaction as the change; telemetry counted only from SDK
      telemetry into hourly rollups; rollout bucketing unchanged
    - Migrations append-only and compatible with the previous release during a rolling deploy
    - Fit for a small shared database (5 connections, 512 MB): bounded tables, retention for
      anything that grows, no per-row fan-out
    - The change matches the spec/plan it claims to implement; deviations are justified
    - Out-of-band steps (secrets, scope registration, otto-platform pin) are identified

    Report: Strengths, Issues (Critical / Important / Minor), Assessment (Approve / Request changes).
```

### Independent security review — `subagent_type: security-auditor`

> **CRITICAL — this review is blind by design.** The security agent receives **only the diff** —
> never the spec, plan, issue, PR body, or implementer's report. Do not paste context into this
> dispatch, and do not ask the agent to read the docs.

```
Agent tool:
  subagent_type: security-auditor
  description: "Independent security review: <subject>"
  prompt: |
    You are the independent security reviewer for PR #<N> on savvagent/otto-flags.

    You receive ONLY the diff — deliberately. Do not read the PR description, the linked issue,
    any design spec or plan, or any implementer summary. Your findings must be derived from the
    code changes alone.

    The diff:
      gh pr diff <N> --repo savvagent/otto-flags

    This is a multi-tenant Rust (axum + sqlx + Postgres) feature-flag service. Agents manage flags
    over an MCP endpoint using OAuth bearer tokens introspected at a separate identity platform;
    running applications evaluate flags over a REST API using SDK keys (public "sdk_" client keys
    and secret "srv_" server keys). Tenant isolation is Postgres row-level security under a
    restricted role. Evaluate the change for security defects, focusing hardest on:
    - Tenant isolation: a query that can read or write another org's rows, a table without RLS, a
      foreign key that lets a row reference another tenant's row, a transaction that escapes the
      restricted role, an org or app id taken from the request and trusted without a check
    - Authentication and authorization: token audience/expiry/scope checks that can be skipped or
      widened, a write reachable with a read scope, an admin action without a fresh role check,
      SDK key resolution that accepts a key for the wrong app or environment, a client key that
      reaches server-only data (targeting rules, other flags, keys)
    - Secrets: SDK keys or secrets stored unhashed, logged, echoed in responses or errors, or
      compared in a timing-unsafe way; a server key that can be read back after creation
    - Webhooks: signature verification that can fail open, replay, or non-idempotent application
    - Untrusted input: SQL built by string formatting, unbounded bodies or arrays, regex or
      parsing on attacker-controlled input, text from SDK telemetry passed to an agent without
      being marked untrusted (prompt injection), database error text in responses
    - Availability on a small shared database: unauthenticated or cheap requests that write rows,
      open connections, or hold streams without limits
    - Data retention: end-user context or PII persisted where it should only be counted

    Report: Strengths, Issues (Critical / Important / Minor), Assessment (Approve / Request changes).
```

Aggregate the trio's reports with the pr-review-toolkit (and any automated reviewer) findings into
one PR comment grouped **Critical / Important / Suggestions / Strengths**. Critical/Important
findings must be fixed or explicitly dismissed with reasoning before merge.

---

## Review-Response Subagent — Phase 4 Step 9 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Address PR review feedback"
  prompt: |
    You are addressing PR review feedback on PR #<N> (savvagent/otto-flags) for <ref>.
    Follow otto-flags-development (Phase 4 step 9).

    Your job:
    - Read all unresolved review threads, including the aggregated trio findings if posted as
      a comment
    - For each comment: fix-and-reply ("Fixed in <sha>") or explicitly dismiss with reasoning.
      NEVER silent dismissal.
    - After each reply, resolve the thread via GraphQL:
        gh api graphql -f query='mutation {
          resolveReviewThread(input: {threadId: "<thread_id>"}) {
            thread { isResolved }
          }
        }'
    - Reply inline to each comment explaining how it was addressed.
    - For automated reviewers, verify a suspected false positive, then dismiss with reasoning.
    - Work in the existing worktree (.claude/worktrees/<dir>) and push to the PR branch — never
      to main. A push to main deploys to production.
    - After any code change, re-run the gates:
        cargo fmt --all -- --check
        cargo clippy --workspace --all-targets -- -D warnings
        DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace
      (plus the pnpm gates if packages/ or examples/ changed). Never edit an existing migration
      to address feedback — add a new one.
    - Commits are conventional, end "Fixes #<n>", and carry no AI attribution; the same for replies.
    - If the same thread stays unresolved across multiple runs, escalate rather than retry.

    Return when all threads are resolved or escalation is needed. The main thread receives only
    the summary (what was fixed, what was dismissed, any escalations).
```
