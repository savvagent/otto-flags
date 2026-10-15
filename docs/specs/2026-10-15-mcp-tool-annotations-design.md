# Annotate every MCP tool as read-only or destructive

**Status:** IMPLEMENTED (#88)
**Issue:** #77

## 1. Brief

From #77, verbatim:

> ## Problem
>
> People are starting to use Otto Flags from Claude (web or desktop) and ChatGPT as a custom connector. Those clients use MCP tool annotations to decide which tools can run without asking and which need the person's confirmation. None of the flags-mcp tools declare any annotations, so a client either asks before every call, including harmless reads, or treats `set_flag_environment` and `rollback_flag`, which change production behaviour, as no riskier than `list_flags`. otto-cxp already annotates its tools (see `crates/cxp-mcp/src/tools/support.rs`).
>
> ## Approach
>
> - Add `annotations(...)` to every `#[tool]` in `crates/flags-mcp/src/tools/`:
>   - Reads (`whoami`, `usage`, `list_apps`, `list_flags`, `get_flag`, `flag_history`, `flag_health`, `evaluate_flag`, `propose_rollback`, `list_rollback_policies`, `rollback_events`) get `read_only_hint = true`.
>   - Writes get `read_only_hint = false`, plus `destructive_hint = true` where the change can switch off or alter live behaviour or delete something: `set_flag_environment`, `rollback_flag`, `archive_flag`, `remove_rollback_policy`, `rotate_app_keys`, `update_flag`, `set_rollback_policy`.
>   - Purely additive tools (`create_flag`, `create_app`) get `destructive_hint = false`.
>   - Set `idempotent_hint` where it is true.
> - Add a test that lists the tools and fails on any tool without annotations, so a new tool can't ship without them.
> - Add a test that pins the read-only set.
>
> ## Acceptance criteria
>
> - `tools/list` shows annotations on every tool.
> - The read-only set exactly matches the tools that don't need `flags:write`.
> - The new test fails when an unannotated tool is added.
>
> ## Non-goals
>
> - Server-side confirmation prompts.
> - Scope changes (consent narrowing is savvagent/otto-platform#43).

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

- **`idempotent_hint` only where a repeat has no further effect the caller can see.** Every tool but
  `propose_rollback`, reads included, records a usage row (`Meter::charge`, `crates/flags-core/src/usage.rs`); a
  non-billable row is internal metering, not an effect, or no read could be read-only. A repeat of
  a billable tool (`flags_core::usage::BILLABLE`) is billed again, and most of them also add a flag
  version or an audit row, so they get `idempotent_hint = false`. `remove_rollback_policy` is not
  billable and a repeat deletes nothing (`remove_policy` returns `false` and writes no audit row;
  only its non-billable usage row repeats), so it gets `idempotent_hint = true`. It is the only one.
- **Every write sets all three hints explicitly** (`read_only_hint`, `destructive_hint`,
  `idempotent_hint`), so no client falls back to the spec defaults (`destructive_hint` defaults to
  true, which would mislabel the additive tools). Read tools set only `read_only_hint = true`; the
  MCP spec says the other hints are meaningful only when `readOnlyHint` is false.
- **`open_world_hint` and `title` are left unset**: the issue does not ask for them, and otto-cxp
  (`crates/cxp-mcp/src/tools/*.rs` in that repo, checked 2026-10-15) does not set them.
- **`update_flag` is destructive** per the issue: replacing variations or configuration changes
  what live evaluations return.
- **Test database on port 15444, not 15434.** The `test` gate's command names the shared
  container on 15434; several agents run `#[sqlx::test]` concurrently, whose database names
  collide across worktrees, so this worktree uses its own container on 15444 (CLAUDE.md allows "give
  a worktree its own container on another port"). Same command otherwise.
- Every other Run Profile value comes from `CLAUDE.md` > Development Workflow.

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
2. `annotations_match_the_pinned_sets`: the names with `read_only_hint == Some(true)` equal a `READ_ONLY`
   constant (the 11 names), and `DESTRUCTIVE`/`IDEMPOTENT` pin the other two hints, so a
   reclassification shows in review.
3. `read_only_tools_change_nothing`: no `READ_ONLY` tool is in `flags_core::usage::BILLABLE`; by
   source scan of the tool files (`include_str!`), a read-only tool's body never names
   `FLAGS_WRITE`, `APPS_ADMIN`, `require_admin`, or `set_archived`, and every other tool's body
   names `FLAGS_WRITE`, `APPS_ADMIN`, or `set_archived` (archive/restore delegate their scope check
   to `set_archived`, which checks `FLAGS_WRITE`). A body runs from `pub async fn <name>(` to the
   method's closing brace at impl indentation (the first `\n    }\n` after it). Reads do call
   `self.charge` (every tool but `propose_rollback` meters a non-billable usage row), so it is not
   forbidden; "not billed" is the `BILLABLE` check. Added in review: a read body also may not call
   any `flags-core` write method (`.create_app(`, `.set_environment(`, `.remove_policy(`, ...), and
   no `IDEMPOTENT` tool may be billable.
4. Added in review: `tools_list_carries_annotations` in `crates/flags-server/tests/e2e.rs` sends
   a real `tools/list` to `/mcp` and checks every tool's camelCase hints on the wire and that the
   `readOnlyHint: true` set equals `READ_ONLY`.

## 7. Invariants impact

- **MCP auth:** unchanged; annotations are hints to clients, not authorization. Every tool still
  checks its scope; test 3 asserts each write body names a write scope.
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
- A tool annotated read-only that is billable or checks a write scope or admin role: test 3 fails.

## 11. Testing approach

Unit tests only (pure, no database), run by the `test` gate
(`DATABASE_URL=postgres://flags:flags@localhost:15444/otto_flags cargo test --workspace`, the
dedicated Postgres for this worktree), plus `lint`, `format`, `extra`, and the `podman build` gate
(crates changed). Post-deploy: `tools/list` on the live `/mcp`, read-only.

## 12. Risks & open questions

- Connector clients may still prompt on destructive writes by design; that is the point.
- Marking billable writes non-idempotent may make a client more cautious about retries; that is
  correct, since a retry is billed again.
