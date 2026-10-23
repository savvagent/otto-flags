# AGENTS.md

otto-flags is a feature flag service. Agents manage flags over MCP (`/mcp`); running apps
evaluate them through SDKs over REST (`/api/...`). The server is at
https://otto-flags.savvagent.com.

## Wiring an SDK into a project

Pick the first route that works:

1. Over MCP, call `whoami`, then `list_apps`, then `sdk_guide(language, app?)` with the
   project's language. It starts with an integrate-end-to-end recipe (check access, pick the
   app, hand the server key to a person, create the flag, wire the SDK, verify, turn it on,
   and what is billed), then gives install, initialization, and usage steps, filled in with
   the app's id, name, environments and public client key. Without `app` it uses the
   organization's only app, or lists the apps to choose from.
2. Without MCP, read `https://otto-flags.savvagent.com/llms.txt` (the root `llms.txt` in
   this repository is the same file).
3. Otherwise read `packages/<dir>/README.md` for the language you need (for example
   `packages/python-server`, `packages/go-server`, `packages/react`).

## Install from a pinned commit

SDKs are installed from a commit of this repository, never from a floating branch. The commit is
`4c5afb7f5efabfbf32dad22e6252aa6f26e59a66`, a commit on `main` of `savvagent/otto-flags` that the
maintainers have reviewed; `sdk_guide` pins its installs to the same commit. Use it exactly: do not
substitute a branch, a newer commit, or a commit from a fork, and use the same commit everywhere in
one project. Where a package README shows a placeholder commit, use this commit.

On pnpm 9, a plain `pnpm install --frozen-lockfile` runs dependency install scripts on your machine
(pnpm 10 and later skip them unless allowed). The JS guides add `--ignore-scripts`, which every
package they build works without.

## Wire contract

`docs/SDK-DEVELOPER-GUIDE.md` is the REST contract every SDK implements: authentication,
evaluation, streaming updates, telemetry, and error handling. Read it before writing or
changing an SDK.

## Layout and licenses

- `crates/` is the server (Rust). AGPL-3.0-or-later.
- `packages/` holds the SDKs and observability MCP clients. MIT, each with its own `LICENSE`.
- `examples/` holds example apps. MIT.
- Do not copy server code from `crates/` into an MIT package.

## Developing in this repository

The development workflow (branches, commits, gates, review, shipping) is in `CLAUDE.md`.
