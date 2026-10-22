# Agent onboarding: `sdk_guide` tool, served `llms.txt`, AGENTS.md

**Status:** DRAFT
**Ticket:** savvagent/otto-flags#152 (platform convention: savvagent/otto-platform#101)

## Brief

#152: coding agents connected to the otto-flags MCP server can manage flags but get no help wiring
an SDK into their app. Add (1) a read-only, free, no-new-scope MCP tool `sdk_guide(language, app?)`
in `crates/flags-mcp`; (2) a refreshed root `llms.txt` and a new `AGENTS.md`. A comment on #152 adds:
the flags server serves its own `/llms.txt` (unauthenticated, static, no database), from the same
single definition as the repo-root file, with no claim of publication. Install guidance is
commit-pinned git/source routes; nothing is published and the owner does not want visibility yet.

## Premise corrections

- `llms.txt` already exists at the repo root. It is stale: links point at `ottoFlags/otto-flags`
  (the repo is `savvagent/otto-flags`), it omits the Python SDK, and its quick start installs an
  unpublished package by name. This work rewrites it rather than creating it.
- The brief says "git dependencies" for every SDK. That holds for Python, Go and Rust. npm and
  SwiftPM cannot install a sub-directory of a git repo, Maven has no git source, and Gradle needs
  a published artifact: those guides say to build from a checkout of the pinned commit and install
  the local artifact. The commit pin is the common rule.
- The AC "never a `srv_` string" is applied to the tool output as a whole: guide text says "server
  key" in words and never contains the literal prefix, so a test can assert the absence.
- `list_apps` already returns the public `client_key` (`FlagApp.client_key`), so personalisation
  needs no new `flags-core` query: `resolve_app` is enough.

## Assumptions

- Run Profile values are all read from CLAUDE.md; none assumed.
- Languages covered are the flag SDKs under `packages/`: typescript, react, vue, svelte,
  sveltekit, solid, angular, astro, nextjs, remix, node, python, go, java, rust, ios, android.
  The `mcp-*` packages are observability MCP clients, not flag SDKs, and are excluded.
- Guide content is written fresh as Markdown under `crates/flags-mcp/content/` (AGPL, with the
  crate) and compiled in with `include_str!`. The Docker image copies only `crates/`, so content
  must live there.
- The tool needs no database read except `resolve_app` when `app` is given; it always records
  a free usage row via `Flags::charge`, like every sibling tool.
- Commit pin: guides contain the literal placeholder `<commit>` and tell the agent to substitute a
  full commit SHA of `savvagent/otto-flags` main that it has verified. The server cannot know its
  own commit (the Docker build has no git).
- `GET /llms.txt` is served from `flags-server`, not `flags-api`, next to `/healthz`.

## Goal & Success Criteria

An agent with an otto-flags MCP connection can call `sdk_guide` and get a correct, tested route to
install and use the right SDK, personalised with its app. Any agent can fetch `/llms.txt` and
`AGENTS.md` without a connection.

- `sdk_guide` returns install, init, evaluate and (Python, in depth) framework snippets per language.
- Snippet identifiers (package, module, import names) are proven against `packages/*` manifests.
- `/llms.txt` is served unauthenticated and equals the root `llms.txt` byte for byte.
- No guide, `llms.txt` or `AGENTS.md` mentions a registry or says a package is published.
- Existing tool tests (annotations, read-only classification, body scan) cover the new tool.

## Scope

In: tool, guide content for the 17 languages, `/llms.txt` route, root `llms.txt` rewrite,
`AGENTS.md`, docs mention (`docs/SDK-DEVELOPER-GUIDE.md` pointer, `docs/deploy/fly.md` tool list
if it lists tools), tests.
Out: publishing, version bumps, new scopes, a docs MCP server, migrations/SQL, SDK wire changes,
changes to any SDK package.

## Architecture

- `crates/flags-mcp/src/guide.rs` (new): `LANGUAGES: &[Lang]` where `Lang { name, aliases, md }`
  and `md` is `include_str!("../content/guides/<name>.md")`; `render(language, Option<&FlagApp>)
  -> Result<String, GuideError>`. Placeholders `{{APP_NAME}}`, `{{APP_ID}}`, `{{CLIENT_KEY}}` are
  replaced with the app's values, or with `<app-name>`, `<app-id>`, `<client-key>` when no app.
  Lookup is case-insensitive over `name` and `aliases`. `GuideError::Unknown` lists the supported
  names. Pure, no I/O.
- `crates/flags-mcp/src/tools/guide.rs` (new): `sdk_guide` tool in a `guide_router`, registered in
  `tools::router()`, `NAMES`, `READ_ONLY`, and the body-scan source list in `tools/mod.rs`.
  Args `{ language: String, app?: String }`. Flow: `caller` -> `require_scope(FLAGS_READ)` ->
  validate `language` (before opening a transaction, so a typo costs no row; a rejected call
  records nothing, as other argument errors do) -> `tx` -> `charge("sdk_guide")` -> if `app`,
  `resolve_app` -> commit -> `Json(SdkGuideOut { language, guide, app, supported })`.
  `out::SdkGuideOut { language: String, guide: String, app: Option<GuideApp{name,id,clientKey}>,
  supported: Vec<String> }`.
- `crates/flags-mcp/content/llms.txt` (new): the single definition. `flags_mcp::LLMS_TXT:
  &str = include_str!(...)`. The repo-root `llms.txt` is a byte-identical copy enforced by a test
  (GitHub raw URLs and the llms.txt convention need a root file; Docker needs the source in
  `crates/`).
- `crates/flags-server/src/llms.rs` (new): `router() -> Router` with `GET /llms.txt` returning
  `text/plain; charset=utf-8` and `cache-control: public, max-age=3600`. No state, no database.
  Merged in `lib.rs` before the fallback. Non-GET methods get the existing JSON 404/405 behaviour
  of the surrounding router (use `get`; same as `/healthz`).
- `AGENTS.md` (root): short, human- and agent-readable: what this repo is, where `llms.txt`
  lives, how to find the SDK for a language, the commit-pin rule, where the wire contract is.

Public-interface changes (all additive): new MCP tool `sdk_guide`; new route `GET /llms.txt`.

## Invariants impact

- MCP auth: scope `flags:read` is checked first; no new scope, so `KNOWN`/`DEFAULT` and the platform
  registration are unchanged.
- Metering: records a free usage row in the tool's own transaction (`Flags::charge`); nothing billed.
- Tenant isolation: the only data read is `resolve_app` under `begin_live` (RLS under `otto_app`); a
  name that is another org's app resolves to "not found" exactly as `get_flag` does.
- No SQL outside `flags-core`: none added. No `.pool()` use.
- Client keys public: only `client_key` (`sdk_`) is returned; the server key is never read (it is
  stored as a hash and has no field on `FlagApp`).
- SDK-reported text untrusted: none involved; content is static plus the app's own validated name,
  id and key.
- Licensing: content is new text in `crates/` (AGPL); nothing in `packages/` or `examples/` changes.
  Content may paraphrase MIT READMEs (MIT into AGPL is permitted); no server code goes into MIT.
- Unauthenticated `/llms.txt`: a `&'static str`, no database, cheap; fits the small shared database.
- Append-only migrations: no migration.

## Out-of-band

None. No secret, config key, scope, or platform registration. Rob's human-only list is untouched.
Deploy is `ci-auto`; `crates/` changes, so the server redeploys and the changed surface is
verified (see Testing).

## Error handling & edge cases

- Unknown `language`: error listing supported names; no usage row. Empty string is unknown.
- Unknown `app`: the standard "unknown app" error listing apps, as sibling tools.
- Aliases: `js`/`javascript` -> typescript, `node`/`nodejs` -> node, `py` -> python, `golang` -> go,
  `swift` -> ios, `kotlin` -> android, `next` -> nextjs.
- `{{...}}` placeholders with no app: replaced by angle-bracket placeholders, so snippets stay valid
  shape for a human to fill. A test asserts no `{{` survives rendering.
- App names allow letters, digits, `-_.` only, so substitution cannot inject Markdown structure
  beyond inline text; the id is a UUID, the key a generated token.

## Testing approach

- Unit, beside code (`crates/flags-mcp/src/guide.rs`): every language resolves; aliases; unknown
  lists supported; placeholders replaced with and without an app; no `{{` or `srv_` in any
  rendering; **drift tests** read `packages/*` manifests via `env!("CARGO_MANIFEST_DIR")/../../`
  and assert each language's identifiers (npm name, pyproject name, Go module path, Cargo name,
  Maven coordinates, Swift/Android names) appear in both the manifest and the guide; Python's
  import name `otto_flags` exists as `packages/python-server/src/otto_flags/__init__.py`;
  **no-publication test** rejects registry and publication words (see plan) in every guide, in
  `LLMS_TXT`, and in `AGENTS.md`; every guide contains the `<commit>` pin and an init and
  evaluate section heading.
- `tools/mod.rs` tests: add to the existing sets; the annotation, read-only and body-scan tests then
  cover the tool (add `guide.rs` to the body-scan source list).
- `crates/flags-mcp` test: `LLMS_TXT` equals the root `llms.txt` (`include_str!` of the root file
  path is fine in a test, which does not run in the Docker build) and mentions every tool in `NAMES`
  and every language in `LANGUAGES`.
- E2E (`crates/flags-server/tests/e2e.rs`): `sdk_guide` with and without `app` (fields, the app's
  client key present, no `srv_`), unknown language and unknown app errors, missing-scope refusal,
  a usage row recorded and not billable, tool listed with `readOnlyHint`; `GET /llms.txt` returns
  200, `text/plain`, equals `LLMS_TXT`, with no `Authorization` header.
- Gates per CLAUDE.md: cargo test/clippy/fmt, the migrations check, `podman build`. No pnpm gate
  (no `packages/**` change). `llms.txt`/`AGENTS.md` are docs.
- Post-deploy: `/readyz`, boot log markers, then `curl https://otto-flags.savvagent.com/llms.txt` and
  a read-only `sdk_guide` call through the MCP client.

## Risks & open questions

- Guide accuracy per language: the drift tests prove names, not that every command works. The
  implementer verifies commands that can run offline here (pip from a local git checkout, go,
  cargo) and writes the rest from each package's own README and manifest. Unverified commands are
  listed in the PR body.
- `<commit>` pin requires the agent to choose a SHA; the guide says how (`git ls-remote`).
- The existing long API-reference section of `llms.txt` is not preserved wholesale: it duplicates
  `docs/SDK-DEVELOPER-GUIDE.md`, which `llms.txt` links to instead.
