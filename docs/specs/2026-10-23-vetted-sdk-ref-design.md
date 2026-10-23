# Have sdk_guide supply a maintainer-vetted SDK ref instead of asking a person to approve a SHA

**Status:** DRAFT
**Ticket:** savvagent/otto-flags#184 (revises #156, spec `2026-10-23-review-pinned-sha-design.md`)

## Brief

> Every `sdk_guide` install is pinned to a commit of `savvagent/otto-flags`, and the agent picks the
> SHA itself (`git ls-remote ... main`). #156 made the guides, `AGENTS.md` and `llms.txt` tell the
> agent to "ask a person to review and approve the SHA before you pin it". That puts the review on
> every consuming developer, for a repo the maintainers already control. Few will read the diff, so
> the step adds friction without much safety. The agent also still chooses the ref, which is the
> thing #156 was trying to constrain.
>
> The guide supplies a maintainer-vetted ref and the agent pins that instead of choosing one. The
> review happens once, on the maintainer side, when the ref is bumped.
>
> - Add a `{{SDK_REF}}` placeholder to the `sdk_guide` rendering, filled from one constant in
>   `crates/flags-mcp`, and use it where the guides, `llms.txt` and `AGENTS.md` currently say `<commit>`.
> - Remove the "ask a person to review and approve the SHA" sentence and the `REVIEW_SHA` tests that
>   enforce it. Replace them with a test that every `<commit>` is gone from the rendered guides and
>   that the ref is a full 40-character SHA.
> - Keep the other #156 content: skip dependency install scripts on pnpm before 10, and the
>   `main`-of-`savvagent/otto-flags`, not-a-fork rule (now enforced by the maintainer who sets the ref).
> - Add the ref bump to the SDK release step so it cannot go stale. Decide how: a CI check that the
>   ref is an ancestor of `main`, or a documented manual step.
>
> Decisions to make in the PR: SHA or tag as the ref; whether the ref must be bumped in the same PR
> as any SDK change, or on a separate cadence.
>
> Acceptance: `sdk_guide` for every language renders a concrete ref with no `<commit>` left and no
> instruction to have a person approve a SHA; `AGENTS.md` and `llms.txt` agree with the guides; a
> test fails if a guide reintroduces `<commit>` or the ref is not a full SHA;
> `no_guide_mentions_a_registry` still passes. Non-goals: publishing the SDKs; the server-key
> handoff and key rotation steps in the recipe.

## Premise corrections

- "Add the ref bump to the SDK release step": the repo has no SDK release step. No SDK is published
  (`CLAUDE.md`, Ship > release), and `release.yml` triggers on a branch that does not exist. The bump
  is therefore a documented maintainer step next to the constant, backed by tests that keep every
  copy of the ref in agreement (see Decisions).
- `AGENTS.md` and `llms.txt` are static files (`llms.txt` is served byte-identical to the root copy;
  `AGENTS.md` is read from the repository), so they cannot take a placeholder. They carry the literal
  SHA, and a test ties each to the constant.
- The ref cannot be bumped in the same PR as the change it should include: a PR cannot name its own
  squash-merge commit. A ref always names a commit already on `main`.

## Assumptions

- **Ref form: a full 40-hex commit SHA, not a tag.** It matches every install route the guides
  already use (`pip ...@<sha>`, `go get ...@<sha>`, `rev = "<sha>"`, `git checkout <sha>`), a tag
  can be moved, and no tags exist in the repo.
- **Initial ref: `4c5afb7f5efabfbf32dad22e6252aa6f26e59a66`**, the tip of `main` when this work
  started (#183). It already contains everything the current guides describe (iOS and Android
  flag-change streams, #177 and #182). It is a commit that has gone through the merge gate, so the
  maintainers have reviewed it.
- **Bump cadence: a separate, small PR after any merge that changes SDK code or a guide's claims
  about SDK behavior**, not the same PR (the correction above). The constant's doc comment says so.
  Staleness costs consumers only the newer SDK behavior; it never points them at unreviewed code.
- **No CI ancestry check.** The `migrations-append-only` job already fetches full history, so
  `git merge-base --is-ancestor` could run there, but the ticket allows a documented step, a check
  adds a workflow change to a docs-and-content PR, and the failure it would catch (a well-formed SHA
  that is not on `main`) shows up at once in the recipe's verify step. Tests instead enforce shape
  (40 lowercase hex) and agreement (every copy equals the constant).
- **No attribution, no registry language** in any new text (existing hygiene tests enforce the
  second).
- Run Profile values read from `CLAUDE.md`, none assumed.

## Goal & Success Criteria

`sdk_guide`, `AGENTS.md` and `llms.txt` tell an agent to install a specific, maintainer-chosen
commit, and no longer ask the consuming developer to approve a SHA.

- Every rendered guide, for every language, with and without an app, contains the SHA and neither
  `<commit>` nor `{{SDK_REF}}`.
- No guide, `AGENTS.md` or `llms.txt` asks a person to approve a SHA or tells the agent to run
  `git ls-remote` to pick one.
- One constant, `SDK_REF`, is the only place the SHA is chosen; tests fail when `AGENTS.md`,
  `llms.txt` (both copies) or any guide disagrees with it, or when it is not 40 lowercase hex.
- The pnpm install-scripts content from #156 and the "not a fork" rule survive.
- `no_guide_mentions_a_registry` and the other hygiene tests pass.

## Scope

In: `crates/flags-mcp/src/guide.rs` (constant, placeholder, tests), the 17 files in
`crates/flags-mcp/content/guides/`, `crates/flags-mcp/content/llms.txt` and the identical root
`llms.txt`, `AGENTS.md`, the `sdk_guide` tool description in `crates/flags-mcp/src/tools/guide.rs`
if it needs a word changed.

Out: publishing, tags, a CI workflow, the key handoff, SDK code, `packages/*/README.md` (`packages/python-server/README.md` still says `<commit>`; a known leftover).

## Architecture

`guide.rs` gains `pub const SDK_REF: &str` (40-hex, doc comment explains vetting and the bump
procedure). `render` adds `.replace("{{SDK_REF}}", SDK_REF)` beside the other placeholders, so the
ref appears in every rendering regardless of `app`. The guides replace each `<commit>` with
`{{SDK_REF}}` and drop the `ls-remote` block and the review sentence, replacing them with one
paragraph: the install is pinned to this maintainer-reviewed commit of `savvagent/otto-flags`, use it
exactly, do not substitute a branch, a newer commit, or a commit from a fork. `llms.txt` and
`AGENTS.md` carry the literal SHA in the same words.

Public interface: the `sdk_guide` output text changes (additive: a concrete value where a
placeholder was). No tool name, argument, or wire change. The module-level docs and the
`install_line_problems` checks move from `<commit>` to `{{SDK_REF}}` for raw guide text.

## Invariants impact

None touched: no SQL, auth, tenant, SDK-path or wire-contract change. Licensing: all edits are in
`crates/` and repo-level files (AGPL), no server code copied into an MIT package. The `sdk_guide`
tool stays free, read-only and static (no database, no network).

## Migration

None.

## Out-of-band

None. No secret, scope, config key or registration. The change redeploys server code
(`crates/flags-mcp`), so the deploy is verified as for any server change; no human-only step.

## Error Handling & Edge Cases

- A rendering that still contains `{{SDK_REF}}` would hand an agent a literal placeholder: a test
  renders every language with and without an app and asserts none remains.
- Markdown is hard-wrapped, so any sentence assertion uses the existing whitespace-normalising
  `flat` helper.
- The root `llms.txt` and `content/llms.txt` must stay byte-identical (existing test); the SHA is
  edited in both.
- A bad bump (typo, uppercase, short SHA, abbreviated) fails the shape test; a bump that misses a
  copy fails the agreement tests, with a message naming the file.

## Testing Approach

All in `crates/flags-mcp/src/guide.rs` unit tests, run with the `test` gate. New tests:
`sdk_ref_is_a_full_lowercase_sha`, `every_rendering_pins_the_sdk_ref` (all languages, with and
without an app), `no_guide_asks_a_person_to_approve_a_sha_or_to_find_one`, and
`agents_md_and_llms_txt_pin_the_sdk_ref`; the server's e2e test of the rendered python guide asserts the ref instead of `<commit>`. Replaced: the two `REVIEW_SHA` tests. Updated:
`every_guide_has_the_commit_pin_and_the_standard_sections` and the install-line checks
(`<commit>` becomes `{{SDK_REF}}` in raw guide text). The server's `/llms.txt` test compares the
served body to `LLMS_TXT`, so it follows the file.

## Risks & Open Questions

- **Stale ref.** Consumers get older SDK code until the ref is bumped. Mitigated by the documented
  bump after SDK or guide-claim changes; the failure is bounded to "older reviewed code".
- **A guide describes behavior newer than the ref.** The current guides are consistent with the
  initial ref. Future guide PRs that describe new SDK behavior must be followed by a bump; the doc
  comment says so, but nothing enforces it.
- **Tests cannot prove the ref is on `main`.** Reviewed by the maintainer at bump time; a typo that
  is well-formed but not a real commit would break installs, which the end-to-end recipe's verify
  step would surface.
