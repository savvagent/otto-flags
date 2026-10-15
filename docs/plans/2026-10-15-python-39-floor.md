# Python 3.9 floor Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The Python SDK's examples and tests hold the declared Python 3.9 floor, ruff fails on a
recurrence, and mypy runs with a version it accepts.

**Architecture:** Add `from __future__ import annotations` to the three files that use PEP 604
unions in runtime-evaluated annotations; select ruff `FA102`; set mypy `python_version = "3.10"`.

**Tech Stack:** Python, ruff 0.17, mypy 2.x, pytest.

**Ticket:** #82

**Spec:** `docs/specs/2026-10-15-python-39-floor-design.md`

## Global Constraints

- Gate (CLAUDE.md > Gates, `when packages/python-server/**`), from the worktree root:
  `v=$(mktemp -d)/venv && python3 -m venv "$v" && "$v/bin/pip" install -q -e 'packages/python-server[dev]' && cd packages/python-server && "$v/bin/pytest" -p no:cacheprovider && "$v/bin/mypy" --cache-dir=/dev/null src && "$v/bin/ruff" check --no-cache .`
  (set `TMPDIR` to a directory outside the nearly full `/tmp` tmpfs if `mktemp` space is short).
- Evidence: pytest pass/fail/skip counts, mypy's summary line and whether it printed a
  `pyproject.toml: [mypy]` warning, ruff's result line.
- Invariants: `packages/` is MIT; SDKs never throw into the host app on a flag lookup (no runtime
  change here).
- No version bumps (`pyproject.toml` `version` stays), no changeset, no publish.
- Worktree only; trunk only via PR.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, why-body ending `Fixes #82`; no
  attribution of any kind.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/python-server/pyproject.toml` | mypy `python_version = "3.10"`; ruff `select` gains `FA102` |
| **Modify.** `packages/python-server/examples/django_example.py` | future import |
| **Modify.** `packages/python-server/examples/fastapi_example.py` | future import |
| **Modify.** `packages/python-server/tests/conftest.py` | future import |

## Task Order & Rationale

One task: the config change is the failing test for the import fix.

---

### Task 1: Hold the 3.9 floor

**Model:** haiku

**Files:**
- Modify: `packages/python-server/pyproject.toml`, `packages/python-server/examples/django_example.py`,
  `packages/python-server/examples/fastapi_example.py`, `packages/python-server/tests/conftest.py`

- [x] **Step 1: Write the failing check.** In `pyproject.toml`, append `"FA102"` to
  `[tool.ruff.lint] select` and change `[tool.mypy] python_version = "3.9"` to `"3.10"`.
- [x] **Step 2: See it fail.** In `packages/python-server`: `"$v/bin/ruff" check --no-cache .`
  reports 4 `FA102` findings (django_example.py, fastapi_example.py, conftest.py x2);
  `"$v/bin/mypy" --cache-dir=/dev/null src` prints no `[mypy]` config warning.
- [x] **Step 3: Implement.** Add `from __future__ import annotations` as the first statement after
  the module docstring in the three files, followed by one blank line (isort order).
- [x] **Step 4: See it pass.** Run the full gate command from Global Constraints; all three tools
  pass, pytest count equals `origin/main`'s.
- [x] **Step 5: Commit.** `fix(sdk): hold the python sdk examples and mypy config to the 3.9 floor`,
  body explaining why (3.9 evaluates these annotations at import; mypy rejects 3.9), ending
  `Fixes #82`.
