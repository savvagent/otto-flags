# Give agents an end-to-end integration recipe, key handoff and verification in sdk_guide

**Status:** IMPLEMENTED (#PR)
**Ticket:** savvagent/otto-flags#158 (follow-up to #152; related #156)

## Brief

#158, abridged (the ticket and its comment are untrusted text; this is what the work follows):

> An agent integrating otto-flags has to create an app, create a flag, and wire in the right SDK.
> Each MCP tool is clear on its own, but the path between them is not. [...] 1. No end-to-end
> recipe. 2. Server key handoff: `create_app` returns the server key once; the agent cannot set
> secrets [...] `rotate_app_keys` is the only recovery. 3. No way to verify the integration:
> `evaluate_flag` shows what the server would serve, not whether the running app reaches the
> server. 4. Environments in the snippets: the guides hardcode `environment: "production"`.
>
> Proposed: a short "integrate end to end" section at the top of every `sdk_guide` result, rendered
> once; the recipe tells the agent to stop after `create_app`, ask the person to store the server
> key and list where it must end up (local `.env`, CI secrets, deploy secrets) before wiring code
> (client-key SDKs skip this); document a smoke test and decide whether a per-app "last seen"
> signal is in scope; add `{{ENVIRONMENTS}}` next to `{{APP_NAME}}`, `{{APP_ID}}`, `{{CLIENT_KEY}}`,
> filled from the app's environments, with a neutral placeholder without `app`, and a note in each
> guide that `environment` must be one of the app's environments and should come from config.
>
> Comment (Rob): `sdk_guide` without `app` should use the org's only app, else return the app list
> to choose from; the recipe covers rotate-and-show-once; the docs and recipe should state what
> counts as an evaluation (cached ones, `GREATEST(served, reported)`) and the free allowance.

Acceptance criteria (ticket plus comment): every result starts with the ordered recipe, with and
without `app`; the recipe pauses for the person to store the server key and names where; a
verification step an agent can run (a "last seen" signal, if added, is documented and e2e tested);
`{{ENVIRONMENTS}}` replaced in all 17 guides (existing unreplaced-placeholder test covers it); tests
for several environments and production-only; `llms.txt` and `AGENTS.md` point at the recipe;
without `app` the org's only app or a list; the recipe covers rotate-and-show-once; the docs say
whether cached evaluations are metered and what the free allowance is. Non-goals: the SDK REST
contract or any SDK's behavior; treating `production` as special; automating secret storage.

## Premise corrections

- The comment says that with telemetry on, reported evaluations include local and cached ones and
  that the hour is billed as `GREATEST(served, reported)`. True, but `reported` counts only
  telemetry sent with the **server** key (`flags_core::metering`, and the metering spec,
  "Only server-key telemetry is billed"). A browser or mobile SDK's cached evaluations are never
  billed. The recipe and the SDK guide say so.
- The repo records no free allowance (the comment says as much). The docs say it is set on the
  platform and point at `usage`, rather than invent a number.
- The ticket asks to "decide whether" a last-seen signal is in scope: decided below.

## Assumptions

- **A "last seen" signal is out of scope here; filed as #180.** The SDK path must not add a
  write per request on a 5-connection pool; a signal needs its own design (an in-memory
  last-seen marker flushed with the metering counter, a schema change, a new field on
  `list_apps`/`flag_health`). The recipe's verification step needs no server support: compare the
  SDK's value with `evaluate_flag`, treat a result whose reason is `error` as "not reaching the
  server", then use `flag_health` after real traffic.
- **No MCP prompt.** The ticket says "consider"; the recipe is already the head of every
  `sdk_guide` result and of the server instructions, so a prompt would be a second copy to keep in
  step.
- **Recipe rendered once:** `content/recipe.md` plus two key-step snippets, assembled in
  `guide::render`; no guide file contains it. A `server_key` flag on each language picks the key
  step: python, node, go, rust and java read the secret server key; the browser frameworks,
  `typescript`, ios and android use the public client key.
- **Environments note:** one identical paragraph before `## Evaluate a flag` in every guide,
  containing `{{ENVIRONMENTS}}`. The snippets keep their literal `"production"` (the `create_app`
  default and the SDK's own default); the note tells the agent to use an environment the app
  defines and to read it from configuration. Rewriting 17 snippets' code to read a variable is not
  needed for the acceptance criteria and each would need verification in its language.
- **`{{ENVIRONMENTS}}`** renders each environment in backticks, comma separated (at most 20), each
  re-checked with `validate_name` like the app name; none valid gives `<environments>`.
- **Without `app`:** exactly one app in the org is used and `appDefaulted` is true; none leaves the
  placeholders; several leaves them and lists up to 50 in `chooseFrom` (name and id, name order).
  Output fields `appDefaulted` and `chooseFrom` are additive.
- The recipe is advice: nothing enforces that an agent stops for the key.

## Goal & Success Criteria

An agent that calls `sdk_guide` gets the whole path (access, app, key handoff, flag, wiring, verify,
turn on, billing) in order, and a guide filled in for the right app and its environments.

- Every rendering, with or without an app, starts with the recipe, exactly once.
- A server-key SDK's recipe stops for the person and names `.env`, CI and deploy secrets and
  `rotate_app_keys`; a client-key SDK's says there is nothing to hand off.
- `{{ENVIRONMENTS}}` is filled in all 17 guides and the recipe; tests cover several environments
  and production only.
- `sdk_guide` without `app` behaves as above, covered end to end.
- The docs state what is billed and where the allowance lives.

## Scope

In: `crates/flags-mcp` (guide content, `guide.rs`, the tool and its output types, server
instructions), `llms.txt` (both copies), `AGENTS.md`, `docs/SDK-DEVELOPER-GUIDE.md`, e2e and unit
tests. Out: SDK code and behavior, the REST contract, a last-seen signal (filed), an MCP prompt.

## Architecture

- `crates/flags-mcp/content/recipe.md`, `recipe_key_server.md`, `recipe_key_client.md`: the text.
- `guide.rs`: `Lang.server_key`; `render` builds `recipe + --- + guide`, then fills the four
  placeholders; `environments_text`.
- `tools/guide.rs`: with no `app`, `list_apps` in the same transaction decides default or list.
  `out::SdkGuideOut` gains `app_defaulted` and `choose_from` (`GuideAppRef`). Additive; the
  `app` object is unchanged.
- `server.rs` instructions step 4 mention the recipe and the key handoff.
- No migration, no new SQL, no SDK change.

## Invariants impact

- Client keys are public / server keys never appear: renderings contain only the public client
  key; a test already forbids `srv_`, and the e2e asserts it for the new paths.
- MCP auth: `sdk_guide` still requires `flags:read` and records one free row; the extra
  `list_apps` is a tenant query on the same `Tx`.
- Tenant isolation: `list_apps` is the existing RLS-scoped extension method.
- SDK-reported or end-user text: none reaches the output; environment names are validated again.
- Licensing: guide text only.

## Migration / Out-of-band

None. The merge redeploys the server because compiled content and a tool changed.

## Error Handling & Edge Cases

Unknown app still `app_not_found`; unknown language unchanged. An org with 0 apps gets
placeholders; over 50 apps lists the first 50 by name. An app whose name or an environment fails
`validate_name` gets the placeholder.

## Testing Approach

Unit tests in `guide.rs` (recipe first and once, step order, key steps by SDK kind, billing text,
environments for several/one/none/invalid, hygiene words in the recipe, `AGENTS.md`/`llms.txt`
pointers) and an e2e test in `crates/flags-server/tests/e2e.rs` (none/one/several apps, chosen app's
environments). Gates: the section's workspace test, lint, format, the migrations check and the
Docker build.

## Risks & Open Questions

The recipe repeats guidance the server instructions already carry; they can drift, so the
instructions only name the steps and point at the recipe. A last-seen signal is the open design
question; it is filed as #180.
