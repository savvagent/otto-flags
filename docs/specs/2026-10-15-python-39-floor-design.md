# Python SDK: hold the declared Python 3.9 floor

**Status:** DRAFT
**Issue:** #82

## 1. Brief

From #82 (found reviewing #19):

> The Python SDK (`packages/python-server`) declares `requires-python = ">=3.9"`, but nothing
> checks 3.9 and two things are already wrong:
>
> - `examples/django_example.py` has a module-level `_client: FlagClient | None = None` and no
>   `from __future__ import annotations`. Module-level annotations are evaluated at import, so
>   importing the example on Python 3.9 raises `TypeError`. Ruff does not flag it (target `py39`,
>   no UP rule covers it).
> - `[tool.mypy] python_version = "3.9"` is rejected by current mypy ("Python 3.9 is not
>   supported (must be 3.10 or higher)"); mypy then checks against its own default, so the gate's
>   mypy run does not check 3.9 semantics.
>
> **Acceptance criteria**
> - [ ] The examples import cleanly on the lowest supported Python (add the future import, or
>   `Optional[...]`).
> - [ ] The mypy config names a version current mypy accepts, or the supported-Python floor is
>   raised deliberately; the gate prints no config warning.
>
> **Out of scope**
> - A CI job for the Python SDK.

## 2. Premise corrections

- **Two more files have the same defect.** `examples/fastapi_example.py:23`
  (`client: AsyncFlagClient | None = None`, module level) and `tests/conftest.py:51-52`
  (`dict | None`, `str | None` in a function signature, evaluated when the `def` runs) also use
  PEP 604 unions without the future import. On 3.9 the FastAPI example fails to import and the
  test suite fails to collect. Every module under `src/otto_flags` that uses `X | Y` already has
  the future import.
- **Ruff can flag it.** No `UP` rule does, but `FA102` (flake8-future-annotations,
  "future-required-type-annotation") reports exactly this: PEP 604 or PEP 585 syntax in an
  annotation that the target version (`py39`) would evaluate at runtime, without the future
  import. Run on `origin/main` (ruff 0.17.1, `ruff check --select FA102 .`), it reports 4
  findings: the two examples and the two `conftest.py` parameters.

## 3. Assumptions

- **Keep the 3.9 floor.** Raising `requires-python` is a support decision for Rob (3.9 is past
  upstream end of life, but nothing in the issue asks to drop it). This change makes the code hold
  the declared floor; it does not change it.
- **mypy `python_version = "3.10"`**, the lowest version current mypy (2.4.0) accepts. Removing
  the key was rejected: mypy would then check against the interpreter running it (3.14 on the dev
  host), further from the floor. mypy cannot check 3.9 semantics at all any more; the residual
  gap is recorded under Risks.
- **Fix with the future import**, not `Optional[...]`: it matches every module in `src/`, keeps
  the modern spelling ruff's `UP` rules ask for, and is the fix `FA102` suggests.
- **Add `FA102` to the ruff selection** (not the whole `FA` group). `FA100` would also ask for the
  future import in files that use `typing.Optional`, which is style, not a 3.9 defect. With ruff
  already in the Python gate (#83), this makes the gate fail on a recurrence.

## 4. Goal & success criteria

The Python SDK's examples and tests contain no annotation that Python 3.9 cannot evaluate, the
gate fails if one is added, and mypy runs with a version it accepts.

- `ruff check --no-cache .` in `packages/python-server` passes with `FA102` selected; before the
  fix it reports 4 `FA102` findings.
- `mypy --cache-dir=/dev/null src` prints no `pyproject.toml: [mypy]` warning and finds no issues.
- `pytest` passes with the same count as on `origin/main`.

## 5. Scope

**In:** the `from __future__ import annotations` line in `examples/django_example.py`,
`examples/fastapi_example.py`, and `tests/conftest.py`; `[tool.mypy] python_version` and
`[tool.ruff.lint] select` in `packages/python-server/pyproject.toml`.

**Out:** a CI job for the Python SDK; raising `requires-python` or the classifiers; running the
suite on a real 3.9 interpreter (none on the dev host, and CLAUDE.md says not to install
toolchains); `examples/python-server` (a separate example with no gate).

## 6. Architecture

No runtime code changes. `from __future__ import annotations` (PEP 563) stores annotations as
strings, so `FlagClient | None` at module level and `dict | None` in a signature are never
evaluated at runtime. The Django example and `conftest.py` never read their annotations at
runtime. The FastAPI example does: FastAPI inspects each route handler's signature when the
`@app.get` decorator runs, resolving string annotations with `typing.get_type_hints` against the
module globals. Its handlers use `str`, `Request`, and the module-level `Annotated` aliases
`OttoFlagsClient` and `RequestContext` (`examples/fastapi_example.py:70-71`), all defined before
the handlers, so they resolve and the `Depends(...)` metadata survives. Importing the module
proves it, since the routes are registered at import. No public interface changes.

## 7. Invariants impact

- **Licensing:** `packages/` stays MIT; no server code involved.
- **SDKs never throw into the host app on a flag lookup:** untouched (no runtime change).
- Every other invariant concerns the server and is untouched.

## 8. Migration

None.

## 9. Out-of-band

None. No version bump, no changeset (pnpm-only), no publish.

## 10. Error handling & edge cases

- The future import must be the first statement after the module docstring, or Python raises
  `SyntaxError`; ruff's isort (`I`) rules keep it in the future-import block.
- In the FastAPI example, an annotation naming something not in module globals (a local alias)
  would fail to resolve under the future import; none does. Verified by importing the example
  with `fastapi` installed in a scratch venv (not a gate: `fastapi` and `django` are not dev
  dependencies) and checking the routes carry their `Depends` dependencies.

## 11. Testing approach

The Python gate (CLAUDE.md > Gates, `when packages/python-server/**`) is the test: `pytest`, `mypy
src`, and `ruff check` (now with `FA102`). Failing first: add `FA102` to the selection and see 4
findings, then add the imports and see it pass. The mypy warning is checked by grepping the mypy
output for `[mypy]`.

## 12. Risks & open questions

- mypy no longer checks 3.9 semantics (stdlib APIs added in 3.10+ would not be caught), and no
  gate runs a 3.9 interpreter. Closing that gap needs a 3.9 interpreter in a gate or CI (out of
  scope), or raising the floor (Rob's call).
