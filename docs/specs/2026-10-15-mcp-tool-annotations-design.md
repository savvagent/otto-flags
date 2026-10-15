# Annotate every MCP tool as read-only or destructive

**Status:** DRAFT
**Issue:** #77

## 1. Brief

From #77:

> People are starting to use Otto Flags from Claude (web or desktop) and ChatGPT as a custom
> connector. Those clients use MCP tool annotations to decide which tools can run without asking
> and which need the person's confirmation. None of the flags-mcp tools declare any annotations,
> so a client either asks before every call, including harmless reads, or treats
> `set_flag_environment` and `rollback_flag`, which change production behaviour, as no riskier
> than `list_flags`.
>
> **Approach**
> - Add `annotations(...)` to every `#[tool]` in `crates/flags-mcp/src/tools/`: reads get
>   `read_only_hint = true`; writes get `read_only_hint = false`, plus `destructive_hint = true`
>   where the change can switch off or alter live behaviour or delete something; purely additive
>   tools (`create_flag`, `create_app`) get `destructive_hint = false`; set `idempotent_hint`
>   where it is true.
> - Add a test that lists the tools and fails on any tool without annotations.
> - Add a test that pins the read-only set.
>
> **Acceptance criteria**
> - `tools/list` shows annotations on every tool.
> - The read-only set exactly matches the tools that don't need `flags:write`.
> - The new test fails when an unannotated tool is added.
>
> **Non-goals:** server-side confirmation prompts; scope changes.

## 2. Premise corrections

- **Two live tools are missing from the issue's lists.** `update_app` (replaces an app's
  environments) and `restore_flag` (un-archives a flag) exist (`crates/flags-mcp/src/tools/mod.rs`
  `NAMES`). Both are writes and both are destructive by the issue's own rule: `update_app` can drop
  an environment, and `restore_flag` brings back a flag that may be on, changing live evaluation
  (archived flags evaluate as off everywhere).
- **"Tools that don't need `flags:write`" is broader than the read set.** `create_app`,
  `update_app`, and `rotate_app_keys` need `apps:admin`, not `flags:write`. The criterion is read
  as "tools that need no write scope (`flags:write` or `apps:admin`)", which is exactly the issue's
  read list. The test checks it that way.

## 3. Assumptions

- **`idempotent_hint` only where a repeat has no further effect at all.** Every billable tool
  (`flags_core::usage::BILLABLE`) records a usage event on every call, and most writes also add a
  flag version, so a repeat is not effect-free: they get `idempotent_hint = false`.
  `remove_rollback_policy` is not billable and a repeat deletes nothing (`remove_policy` returns
  `false`, writes no audit row), so it gets `idempotent_hint = true`. It is the only one.
- **Every write sets all three hints explicitly** (`read_only_hint`, `destructive_hint`,
  `idempotent_hint`), so no client falls back to the spec defaults (`destructive_hint` defaults to
  true, which would mislabel the additive tools). Read tools set only `read_only_hint = true`; the
  MCP spec says the other hints are meaningful only when `readOnlyHint` is false.
- **`open_world_hint` and `title` are left unset**: the issue does not ask for them, and otto-cxp
  does not set them.
- **`update_flag` is destructive** per the issue: replacing variations or configuration changes
  what live evaluations return.
- Run Profile: every value comes from `CLAUDE.md` > Development Workflow; none assumed.

## 4. Goal & success criteria

Every tool on `/mcp` declares annotations that let a connector client auto-run reads and ask
before changes that alter live behaviour.

- `tools/list` (rmcp's `ToolRouter::list_all`) returns `annotations` on all 22 tools.
- The tools with `readOnlyHint: true` are exactly the 11 read tools, pinned by a test.
- A test fails, naming the tool, when any routed tool has no annotations or a write leaves a hint
  unset.
- A test cross-checks the classification against the code: no read-only tool is billable or
  checks a write scope, and every write tool checks one.

## 5. Scope

**In:** `annotations(...)` on the 22 `#[tool]`s; unit tests in `crates/flags-mcp/src/tools/mod.rs`.

**Out:** confirmation prompts, scope changes, tool descriptions, `open_world_hint`, the SDK REST
API.

## 6. Architecture

Classification (the single definition is the attribute on each tool; the test pins it):

| Tool | file | readOnly | destructive | idempotent |
|---|---|---|---|---|
| whoami, usage | org.rs | true | | |
| list_apps | apps.rs | true | | |
| list_flags, get_flag | flags.rs | true | | |
| flag_history, evaluate_flag, flag_health | insight.rs | true | | |
| list_rollback_policies, propose_rollback, rollback_events | rollback.rs | true | | |
| create_app | apps.rs | false | false | false |
| update_app, rotate_app_keys | apps.rs | false | true | false |
| create_flag | flags.rs | false | false | false |
| update_flag, set_flag_environment, archive_flag, restore_flag, rollback_flag | flags.rs | false | true | false |
| set_rollback_policy | rollback.rs | false | true | false |
| remove_rollback_policy | rollback.rs | false | true | true |

rmcp 2.0.0's `#[tool(annotations(...))]` serializes these as `annotations.readOnlyHint`,
`destructiveHint`, `idempotentHint` on each `tools/list` entry. Public-interface change: additive
(a new optional field on every tool definition); no input or output schema changes.

Tests in `crates/flags-mcp/src/tools/mod.rs` `tests`, beside the existing routing test:

1. `every_tool_is_annotated`: for each tool in `router().list_all()`, `annotations` is `Some`
   (panic names the tool), `read_only_hint` is `Some`; when it is `Some(false)`,
   `destructive_hint` and `idempotent_hint` are `Some`.
2. `read_only_set_is_pinned`: the names with `read_only_hint == Some(true)` equal a `READ_ONLY`
   constant (the 11 names), and `DESTRUCTIVE`/`IDEMPOTENT` pin the other two hints, so a
   reclassification shows in review.
3. `read_only_tools_change_nothing`: no `READ_ONLY` tool is in `flags_core::usage::BILLABLE`; by
   source scan of the tool files (`include_str!`), a read-only tool's body never names
   `FLAGS_WRITE`, `APPS_ADMIN`, `self.charge`, or `set_archived`, and every other tool's body names
   `FLAGS_WRITE`, `APPS_ADMIN`, or `set_archived` (archive/restore delegate their scope check to
   `set_archived`, which checks `FLAGS_WRITE`).

## 7. Invariants impact

- **MCP auth:** unchanged; annotations are hints to clients, not authorization. Every tool still
  checks its scope; test 3 asserts the write-scope checks are present.
- **Metering:** unchanged; test 3 reads `BILLABLE` without modifying it.
- All other invariants (migrations, RLS, SQL placement, SDK contract, licensing): untouched.

## 8. Migration

None.

## 9. Out-of-band

None: no config key, secret, scope, or platform registration. Connector clients pick up the
annotations on their next `tools/list`.

## 10. Error handling & edge cases

- A new tool added without `annotations(...)`: test 1 fails naming it (and the existing routing
  test fails until it is in `NAMES`).
- A new read tool annotated read-only but not added to `READ_ONLY`: test 2 fails.
- A tool annotated read-only that charges or checks a write scope: test 3 fails.

## 11. Testing approach

Unit tests only (pure, no database), run by the `test` gate
(`DATABASE_URL=postgres://flags:flags@localhost:15444/otto_flags cargo test --workspace`, the
dedicated Postgres for this worktree), plus `lint`, `format`, `extra`, and the `podman build` gate
(crates changed). Post-deploy: `tools/list` on the live `/mcp`, read-only.

## 12. Risks & open questions

- Connector clients may still prompt on destructive writes by design; that is the point.
- Marking billable writes non-idempotent may make a client more cautious about retries; that is
  correct, since a retry is billed again.
