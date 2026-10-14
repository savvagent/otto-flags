# Answer MCP tool arguments containing NUL without a failed statement

**Status:** DRAFT
**Issue:** #61

## 1. Brief

From #61 (found by the reviews of #60):

> #60 makes a flag key containing NUL an unknown key everywhere a flag is looked up by key. Other
> MCP tool arguments that reach a query unchanged can still contain NUL: for example the app
> `name` (or id) that every tool resolves, `environment` arguments, and the change `reason`.
> Postgres `text` rejects NUL (`invalid byte sequence for encoding "UTF8": 0x00`), so such an
> argument fails its statement and the tool answers `internal_error` and logs a server-side
> failure, instead of the not-found or invalid-input error it should get.
>
> These come from authenticated agents, not public keys, so this is about correct errors and clean
> logs, not exposure.
>
> **Acceptance criteria**
> - [ ] Every MCP tool argument bound into a query either finds nothing (lookups: app,
>   environment) or is refused as invalid input (writes: names, descriptions, reasons) when it
>   contains NUL, with no statement that fails on it.
> - [ ] An e2e test calls at least one lookup tool and one write tool with NUL in an argument and
>   gets the expected non-internal error code.

## 2. Premise corrections

- **`jsonb` rejects NUL too, and JSON arguments reach it.** Postgres refuses the escape `\u0000`
  in `jsonb` input (`unsupported Unicode escape sequence`, SQLSTATE 22P05), so a NUL anywhere in a
  string or object key of `create_flag`/`update_flag` `variations` or `configuration`, or in a
  `set_flag_environment` rule (`attribute`, `values`, `description`), fails the write as
  `internal_error` today. These are write arguments and are refused as invalid input like the
  text ones.
- **Several arguments are already safe** because they are validated, or compared in memory,
  before any statement binds them:
  - `create_app` `name`, `create_app`/`update_app` `environments`, and variation names:
    `eval::validate_name` allows only ASCII letters, digits, `-`, `_`, `.` (`invalid_argument`).
  - `create_flag` `key`: `validate_key`, same alphabet. Flag keys in lookups: #60.
  - `set_flag_environment` `environment`: checked against `app.environments` before the lock
    (`apply_env_patch`); `set_rollback_policy` `environment`: `require_environment` before the
    insert. Both `invalid_argument`.
  - `set_flag_environment` `defaultVariation` and a rule's `variation`: must name a variation
    (`validate_env`), so NUL is `invalid_argument`.
  - `evaluate_flag` `environment` and `context`: evaluated in memory, never bound.
  - Every other argument is a number, a boolean, or an enum (`which`, `mode`).
- **`flag_health`'s `environment` has a natural answer, not just "nothing".** Telemetry stores a
  reported environment through `telemetry::stored(s, 64)`, which replaces NUL with U+FFFD and caps
  it at 64 characters (#38). An SDK that reported `"production\0"` has its counts stored under
  `"production\u{FFFD}"`. Normalizing the queried environment the same way finds exactly those
  rows, and binds no NUL. For an environment nothing reported under, it finds nothing, as an
  unknown environment does today.

So the arguments that reach a statement with NUL today are:

| Tool(s) | Argument | Statement | Fix |
|---|---|---|---|
| every tool naming an app | `app` | `resolve_app` | lookup: `app_not_found` |
| `remove_rollback_policy` | `environment` | `remove_policy` DELETE | lookup: `removed: false` |
| `propose_rollback` | `environment` | `find_policy` | lookup: no policy, then the existing `require_environment` refusal (`invalid_argument`), as for any unknown environment |
| `flag_health` | `environment` | `telemetry::health` | lookup: normalized as telemetry stores it |
| `create_flag`, `update_flag` | `name`, `description` | INSERT/UPDATE `feature_flags` | write: `invalid_argument` |
| every flag writer | `reason` | INSERT `flag_versions` | write: `invalid_argument` |
| `create_flag`, `update_flag` | `variations`, `configuration` | INSERT/UPDATE `feature_flags` (jsonb) | write: `invalid_argument` |
| `set_flag_environment` | `rules` | UPDATE `feature_flags.environments` (jsonb) | write: `invalid_argument` |

## 3. Assumptions

- **Guard in `flags-core`, not at the MCP boundary.** A blanket refusal of any tool call whose
  arguments contain NUL would be one check, but it would answer lookups with `invalid_argument`
  where the AC (and #60's `get_flag` test) expect not-found, and it would leave the core functions
  failing for any other caller. Each statement's own function guards its input, as #60 did.
- **Lookups reuse #60's predicate.** `flags::unstorable` (`contains('\0')`) already means "no
  stored text can equal this"; `resolve_app`, `find_policy` and `remove_policy` use it. Its doc
  comment is generalized from "key" to any stored text.
- **Writes refuse NUL; they do not strip or replace it.** Silently changing an agent's reason or
  configuration would store something it did not send. `validate_text` (name, description, reason)
  refuses text containing NUL; a new `eval::contains_nul(&Value)` (any string or object key, at any
  depth) refuses `variations` (in `validate_variations`), rules (in `validate_env`), and
  `configuration` (in `create_flag`/`update_flag`). Message: `"<what> must not contain NUL
  (U+0000)"`.
- **`flag_health` normalizes with telemetry's own function** (`stored`, `MAX_ENVIRONMENT = 64`)
  rather than returning an empty answer: it is the only reading under which the answer is the
  data an SDK reported under that name. A side effect: an environment longer than 64 characters
  now finds the rows its truncated form was stored under, which is also what telemetry recorded
  for it. The answer's `environment` field echoes the normalized name (no raw NUL in output).
  The `64` literal at the two ingest sites becomes the same constant.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

No MCP tool argument containing NUL reaches a statement that fails on it.

- `get_flag` (and every tool) with app `"web\u0000"` answers `app_not_found`, listing the apps.
- `remove_rollback_policy` with environment `"production\u0000"` answers `removed: false`.
- `flag_health` with environment `"production\u0000"` answers health, counting what was reported
  under that environment.
- `create_flag`/`update_flag`/`set_flag_environment`/`archive_flag`/`rollback_flag` with NUL in a
  name, description, reason, variations, configuration, or rule answer `invalid_argument`.
- No `server-side failure surfaced to an MCP caller` is logged for any of these.

## 5. Scope

**In:** `resolve_app`, `find_policy`, `remove_policy`, `telemetry::health`, `validate_text`,
`validate_variations`, `validate_env`, `create_flag`/`update_flag` configuration; tests.

**Out:** error message formatting (`require_environment` prints the environment with `'{}'`, so
its message carries the raw NUL inside the JSON-escaped error string; harmless, unchanged). The SDK
path (#60 covered it). Any argument already validated before a statement (section 2).

## 6. Architecture

`crates/flags-core/src/flags.rs`:
- `unstorable`: doc comment generalized ("text no stored value can equal").
- `validate_text`: `Some(s) if s.contains('\0') => Err(Invalid("{what} must not contain NUL
  (U+0000)"))`, checked before the length check.
- `create_flag`: `if new.configuration.as_ref().is_some_and(eval::contains_nul)` → `Invalid`.
- `update_flag`: the same check on `patch.configuration`, where the patch is applied after the lock,
  like the other fields (the lock binds no argument but the key, which #60 guards).

`crates/flags-core/src/eval.rs`:
- `pub(crate) fn contains_nul(v: &Value) -> bool`: recursive over arrays, objects (keys and
  values) and strings.
- `validate_variations`: refuse `contains_nul(v)` first.
- `validate_env`: per rule, refuse NUL in `attribute`, `description`, or any of `values`
  (`"rule {i}: must not contain NUL (U+0000)"`). Stored environments never contain NUL, so
  `validate_all_envs` and the rollback checker's path are unaffected.

`crates/flags-core/src/apps.rs` `resolve_app`: when `unstorable(wanted)`, skip the query; fall
through to the existing `AppNotFound` with the list of apps (a query with no argument).

`crates/flags-core/src/rollback.rs`: `find_policy` returns `Ok(None)` and `remove_policy` returns
`Ok(false)` for an unstorable environment, without a statement.

`crates/flags-core/src/telemetry.rs`: `const MAX_ENVIRONMENT: usize = 64`, used by both ingest
sites; `health` binds `environment.map(|e| stored(e, MAX_ENVIRONMENT))` and returns that in
`Health::environment`.

No public interface changes: tools answer documented error codes instead of `internal_error`.

## 7. Invariants impact

- **No SQL outside flags-core; runtime queries only:** unchanged; fewer statements.
- **Tenant isolation, `begin_live`:** unchanged; no new statement.
- **SDK-reported text is untrusted:** unchanged; `health` output still passes through `inert`.
- **Evaluations are counted, not stored:** unchanged.
- **Every flag change is versioned:** a refused write changes nothing (the transaction is dropped).
- **Metering:** the charge is recorded in the tool's transaction, which a refusal drops, so a
  refused call is not billed, as for any other `invalid_argument`.

## 8. Migration

None.

## 9. Out-of-band

None.

## 10. Error handling and edge cases

- App argument that is only whitespace plus NUL: trimmed, still contains NUL, `app_not_found`.
- A UUID with a NUL appended does not parse as a UUID; it is not found.
- NUL inside a nested `configuration` object key, or an array inside a rule value: `contains_nul`
  walks the whole tree. serde_json's parse depth limit (128) bounds the recursion.
- `rollback_flag`'s reason is prefixed (`"rollback to vN: <reason>"`) before `validate_text`; the
  NUL survives the prefix and is refused.
- Error messages produced by `AppNotFound`/`FlagNotFound` print the name with `{:?}`, so NUL shows
  as `\0`.

## 11. Testing approach

- Unit (`eval.rs`): `contains_nul` finds NUL in a string, an object key, a nested array, and not
  in clean JSON; `validate_variations` and `validate_env` refuse it.
- DB (`crates/flags-core/tests/db.rs`), `arguments_with_nul_find_nothing_or_are_refused`:
  `resolve_app("web\0")` → `AppNotFound`; `create_flag` with NUL in name, description, reason,
  variations, configuration → `Invalid`; `update_flag` configuration, `set_environment` rule value
  → `Invalid`; `find_policy`/`remove_policy` with `"production\0"` → `None`/`false`;
  `telemetry::health` with `"production\0"` → `Ok`, and counts evaluations recorded under that
  reported environment; after all of it the transaction still commits (no statement failed).
- e2e (`crates/flags-server/tests/e2e.rs`), `mcp_arguments_with_nul_are_not_internal_errors`:
  lookup tools `get_flag` (app NUL → `app_not_found`), `remove_rollback_policy` (environment NUL →
  `removed: false`), `flag_health` (environment NUL → success); write tools `create_flag` (reason
  NUL), `update_flag` (configuration NUL), `set_flag_environment` (rule value NUL) →
  `invalid_argument`.

Gates: test, lint, format, extra, and the `crates/**` podman build.

## 12. Risks and open questions

- Coverage depends on the inventory in section 2; a new tool argument bound unvalidated would
  reopen it. The DB and e2e tests pin today's set.
