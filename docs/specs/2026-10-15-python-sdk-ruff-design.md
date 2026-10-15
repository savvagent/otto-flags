# Python SDK: pass ruff check and gate on it

**Status:** IMPLEMENTED (#83)
**Issue:** #19

## 1. Brief

From #19:

> `ruff check` fails on the Python SDK (`packages/python-server`) with 23 errors under the rules its
> own `pyproject.toml` selects (`E, F, I, N, W, UP, B, C4, SIM`). `ruff` is a declared dev
> dependency, but because of these errors it is left out of the Python gate in `CLAUDE.md`
> (Development Workflow section, added by #13) > Gates. Lint regressions there therefore go
> unnoticed.
>
> **Acceptance criteria**
> - [ ] `ruff check .` passes in `packages/python-server` with the existing rule selection: no new
>   ignores, and no `# noqa` without a one-line reason.
> - [ ] `pytest` and `mypy src` still pass (the existing Python gate).
> - [ ] `CLAUDE.md` > Gates runs `ruff check` in the Python gate, in the same PR.
>
> **Out of scope**
> - `ruff format`, and changes to the rule selection.
> - A CI job for the Python SDK.
>
> **Risks & open items**
> - `pyproject.toml` allows `ruff>=0.1.0`, so new ruff releases can add findings. Pin a minimum
>   version that matches the gate, or accept the drift?
> - B904 changes exception chaining (`__cause__`), which callers can observe. Confirm that no test
>   asserts on it.

## 2. Premise corrections

None. Reproduced on `origin/main` at 4313a31 with ruff 0.17.0: 23 errors, the same rule counts
as the issue's table. 4 of the 23 are in `examples/django_example.py` (all of the E402s), 1 F401
there, and the rest in `src/` and `tests/`.

## 3. Assumptions

- **Ruff lower bound: `ruff>=0.17.0`**, the version the gate was verified with (resolves the
  first open item). A newer ruff can still add findings; the gate then fails loudly and the fix is
  a normal lint change. An upper pin was rejected: it would freeze the SDK's dev tooling and need
  routine bumps nobody owns.
- **B904 uses `raise ... from e`**, not `from None`. Before, the original `TypeError`/`ValueError`
  was attached implicitly as `__context__`; `from e` makes it the explicit `__cause__` (and keeps
  `__context__`). Nothing is lost to a caller; only the traceback banner changes ("direct cause"
  instead of "during handling"). No test in `tests/` refers to `__cause__` or `__context__`, nor
  to this message (grep, resolves the second open item). A new test pins the chaining.
- **E402 in the Django example: move the four Django imports to the top** rather than `# noqa`.
  The file is one module that illustrates several Django files; the section banners stay, and each
  section keeps a one-line comment naming the imports it uses, so the teaching layout survives.
- **SIM105 (2, unsafe-fix in ruff):** `contextlib.suppress(asyncio.CancelledError)` around the
  awaited task is equivalent to `try/except CancelledError: pass`, so applied by hand.
- **SIM114 / SIM117:** applied as ruff's safe fixes; combining `if`-arms with identical bodies and
  nesting-equivalent `with` statements does not change behavior.
  Ruff's SIM114 output (`a == "heartbeat" or a == "connected"`) was then written as
  `in ("heartbeat", "connected")` to match the `elif` beside it (review suggestion, same behavior).
- **Gate command:** append `&& "$v/bin/ruff" check --no-cache .` to the existing Python gate
  (after `cd packages/python-server`), with `--no-cache` so the gate leaves no `.ruff_cache`, like
  `-p no:cacheprovider` and `--cache-dir=/dev/null` already do.
- No version bump (the Python SDK is unpublished; CLAUDE.md > Ship > release). No changeset (not a
  pnpm package). No CHANGELOG entry: lint-only, no user-facing change beyond exception chaining.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

The Python SDK lints clean under its own configuration, and the gate keeps it clean.

- `ruff check --no-cache .` in `packages/python-server` prints `All checks passed!` with ruff
  0.17.0; `pyproject.toml`'s `[tool.ruff]` tables are unchanged.
- No `# noqa` added.
- `pytest` passes (58 before, plus the new chaining tests) and `mypy src` reports no issues.
- The Python gate in `CLAUDE.md` runs `ruff check`; the "23 existing errors ... until #19" note is
  gone.

## 5. Scope

**In:** `packages/python-server/{src,tests,examples}/**/*.py`, `packages/python-server/pyproject.toml`
(ruff lower bound only), `CLAUDE.md` (Python gate line and the note).

**Out:** `ruff format`; rule selection; a CI job; `examples/python-server`; the stale
`__version__ = "1.0.0"` in `__init__.py`; the `mypy` "Python 3.9 is not supported" warning
(mypy still runs and passes).

## 6. Architecture

No design change. Per finding:

| Rule | Where | Fix |
|---|---|---|
| I001 ×7 | `src/otto_flags/__init__.py`, `realtime.py`, `tests/*.py`, Django example | `ruff check --fix` |
| F401 | Django example (`typing.Any`) | `--fix` (removed) |
| UP037 ×3 | `async_client.py:99`, `cache.py:248`, `client.py:102` | `--fix`; each module has `from __future__ import annotations`, so the unquoted self-type is not evaluated at runtime |
| SIM114 ×2, SIM117 ×2 | `realtime.py` (sync and async SSE loops) | `--fix` |
| SIM105 ×2 | `realtime.py:395`, `telemetry.py:433` | `contextlib.suppress(asyncio.CancelledError)` |
| B904 ×2 | `client.py:417`, `async_client.py:416` (`set_config_override`) | `raise ValueError(...) from e` |
| E402 ×4 | Django example | imports moved to the top |

No public interface changes. The exception type and message of `set_config_override` are
unchanged; `__cause__` is now set.

## 7. Invariants impact

- **SDKs never throw into the host app on a flag lookup:** untouched; `set_config_override`'s
  `ValueError` is a documented raise on an explicit override call, not a lookup.
- **Licensing:** only MIT package files change; no server code copied.
- Others (migrations, RLS, SQL, MCP auth, metering) are not touched.

## 8. Migration

None.

## 9. Out-of-band

None. Not published, so nothing to release (Rob's call per CLAUDE.md).

## 10. Error handling and edge cases

- The `with` merge in the SSE loops keeps the same enter/exit order (client, then stream; exit
  reversed), and exceptions from either still reach the surrounding `try`.
- `contextlib.suppress` swallows only `CancelledError`, as before.

## 11. Testing approach

- New tests in `tests/test_client.py` and `tests/test_async_client.py`: `set_config_override` with
  a non-JSON-serializable value raises `ValueError` whose `__cause__` is the original `TypeError`.
  Fails before the B904 change (`__cause__` is `None`), passes after.
- Gate: `when packages/python-server/**` (pytest, mypy, and now ruff), with pass counts.
- No Rust, pnpm, or podman gate matches the changed files (`CLAUDE.md` is not in any `when` glob).

## 12. Risks and open questions

- A future ruff can add findings under `>=0.17.0`; accepted (see Assumptions).
