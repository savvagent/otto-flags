# Python SDK Ruff Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `ruff check` passes in `packages/python-server` under its existing configuration, and
the Python gate in `CLAUDE.md` runs it.

**Architecture:** Lint fixes only (see the spec's per-rule table), plus explicit exception
chaining in `set_config_override` pinned by a test.

**Tech Stack:** Python 3.9+, pytest, mypy, ruff 0.17.0.

**Ticket:** savvagent/otto-flags#19

**Spec:** `docs/specs/2026-10-15-python-sdk-ruff-design.md`

## Global Constraints

- Worktree only; `main` changes only through the PR.
- Gate (from `CLAUDE.md` > Gates, `when packages/python-server/**`), from the worktree root; after
  Task 2 it ends with `&& "$v/bin/ruff" check --no-cache .`:
  `v=$(mktemp -d)/venv && python3 -m venv "$v" && "$v/bin/pip" install -q -e 'packages/python-server[dev]' && cd packages/python-server && "$v/bin/pytest" -p no:cacheprovider && "$v/bin/mypy" --cache-dir=/dev/null src`
- Evidence: pytest pass/fail/skip counts, mypy summary line, ruff summary line, ruff version.
- No new `# noqa`, no change to `[tool.ruff]` / `[tool.ruff.lint]`, no version bump.
- Invariants: SDK lookups never throw (untouched); MIT licensing (only package files change).
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #19`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/python-server/src/otto_flags/{__init__,async_client,cache,client,realtime,telemetry}.py` | lint fixes; `from e` chaining |
| **Modify.** `packages/python-server/tests/{conftest,test_async_client,test_cache,test_client}.py` | import order; chaining tests |
| **Modify.** `packages/python-server/examples/django_example.py` | imports to top; unused import |
| **Modify.** `packages/python-server/pyproject.toml` | `ruff>=0.17.0` |
| **Modify.** `CLAUDE.md` | ruff in the Python gate; drop the #19 note |

## Task Order & Rationale

Task 1 fixes the code (test-first for the one behavior change); Task 2 turns the gate on once it
can pass.

### Task 1: Fix the 23 ruff findings

Model: sonnet

- [ ] Write tests: in `tests/test_client.py` and `tests/test_async_client.py`, call
  `set_config_override("flag", {"bad": object()})` and assert it raises `ValueError` whose
  `__cause__` is a `TypeError`.
- [ ] Run pytest; the two new tests fail (`__cause__` is `None`).
- [ ] `ruff check --fix --no-cache .` in `packages/python-server` (safe fixes only).
- [ ] By hand: `from e` on both B904 raises; `contextlib.suppress(asyncio.CancelledError)` in
  `realtime.py` and `telemetry.py` (import `contextlib` in the stdlib block); move the four
  Django imports to the top of `examples/django_example.py`, leaving each section a comment naming
  the imports it uses.
- [ ] Run pytest (all pass), mypy (no issues), `ruff check --no-cache .` (All checks passed!).
- [ ] Commit: `fix(python-sdk): fix the ruff check findings`.

### Task 2: Gate on ruff

Model: haiku

- [ ] `pyproject.toml`: `"ruff>=0.1.0"` → `"ruff>=0.17.0"`.
- [ ] `CLAUDE.md`: append `&& "$v/bin/ruff" check --no-cache .` to the `when packages/python-server/**`
  gate; delete the sentence "`ruff check` reports 23 existing errors in `packages/python-server`
  and is not part of its gate until #19."
- [ ] Run the full gate command from the worktree root; all three tools pass.
- [ ] Commit: `chore(python-sdk): run ruff check in the python gate`.
