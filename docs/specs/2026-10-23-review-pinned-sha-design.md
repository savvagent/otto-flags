# Tell agents to review the pinned SHA in the SDK guides and note install-script behavior

**Status:** IMPLEMENTED (#PR)
**Ticket:** savvagent/otto-flags#156 (follow-up to #153)

## Brief

> The `sdk_guide` install sections tell an agent to find a commit with `git ls-remote
> https://github.com/savvagent/otto-flags main` and pin it at once. That trusts whatever is on
> `main` at that moment with no human in the loop. The JS guides also run `pnpm install
> --frozen-lockfile` in a fresh clone, which runs every dependency's install scripts on the user's
> machine (the lockfile is pinned, so the risk is low).
>
> - After the `ls-remote` step in each guide, and in `AGENTS.md` and `llms.txt`, say a person
>   should review or approve the chosen SHA, or prefer the SHA of a commit they have already checked.
> - In the JS build blocks, note that the build runs dependency install scripts, or suggest
>   `--ignore-scripts` where the package build does not need them (verify per package before
>   suggesting it).
> - Keep the wording free of registry or publication language (the hygiene tests enforce it).

Acceptance: every guide's Install section, `AGENTS.md` and `llms.txt` carry the review-the-SHA
sentence; a test asserts it in every guide; the JS guides state the install-scripts behavior and
`--ignore-scripts` is suggested only where verified to build. Out of scope: publishing, changing
the install routes.

## Premise corrections

- Whether `pnpm install` runs dependency install scripts depends on the pnpm version: pnpm 10 and
  later skip them unless a package is allowed (`onlyBuiltDependencies`), pnpm 9 runs them. The
  repo's `engines` allows `pnpm >= 9`, and it sets no allow-list, so the guide says exactly that
  rather than claiming scripts always run.

## Assumptions

- One sentence, identical in all 17 guides, `AGENTS.md` and `llms.txt`, so a test can assert it
  verbatim (after whitespace normalisation, since Markdown is hard-wrapped).
- `--ignore-scripts` verification: on a fresh clone, `pnpm install --frozen-lockfile
  --ignore-scripts`, then `pnpm --filter <pkg> build` and `pack` succeeded for all eleven packages
  the guides name (`sdk`, `react`, `vue`, `svelte`, `solid`, `angular`, `astro`, `nextjs`, `remix`,
  `sveltekit`, `node-server`) with pnpm 10.21. So the flag is suggested in every JS guide.
- The JS guides keep one install command; the install-scripts paragraph goes right before the
  code block that contains it.
- The sentence is advice to the agent, not a mechanism: nothing enforces that a person approves.

## Goal & Success Criteria

An agent following `sdk_guide`, `AGENTS.md` or `llms.txt` is told to have a person approve the SHA
it pins, and the JS builds no longer run dependency install scripts by default.

- All 17 guides, `AGENTS.md` and `llms.txt` carry the sentence; tests fail if any loses it.
- All 11 guides that run `pnpm install` use `--ignore-scripts` and say what an install runs.
- The registry/publication hygiene tests still pass.

## Scope

In: `crates/flags-mcp/content/guides/*.md`, `crates/flags-mcp/content/llms.txt` and its identical
root copy, `AGENTS.md`, tests in `crates/flags-mcp/src/guide.rs`. Out: SDK code, the install routes,
publishing, the Go/Python/Java/Swift/Kotlin build commands.

## Architecture

Static content plus three tests in `guide.rs`: `every_guide_tells_the_agent_to_have_the_sha_reviewed`,
`agents_md_and_llms_txt_tell_the_agent_to_have_the_sha_reviewed`, and
`js_guides_skip_dependency_install_scripts_and_say_why`. No public interface change.

## Invariants impact

None touched (no SQL, auth or SDK path). Licensing: guide text only.

## Migration / Out-of-band

None. The merge redeploys the server because the embedded guide and `llms.txt` text changed;
`sdk_guide` serves the new text.

## Error Handling & Edge Cases

Guides are hard-wrapped, so the tests compare whitespace-normalised text. `llms.txt` has two
copies; an existing test requires them byte-identical.

## Testing Approach

`cargo test -p flags-mcp` (guide tests), then the workspace gates and the Docker build, because
`crates/` changed.

## Risks & Open Questions

The advice depends on the agent following it. A guide's wording can drift from the build blocks;
the install-scripts test pins the flag and the explanation.
