# otto-flags SvelteKit SDK (app: {{APP_NAME}})

`@otto-flags/sveltekit` adds server-side helpers for load functions and form actions, and re-exports
the client-side stores of `@otto-flags/svelte`. It wraps `@otto-flags/sdk`; its install includes
that package and `@otto-flags/svelte`.

The client key is public by design: it can ship in a browser bundle and sits in source control
without harm. It can evaluate flags but cannot read targeting rules. Do not put the app's server key
in this package: its client accepts only the public client key, and a server key must never reach a
browser.

The server helpers use the same core client as the browser stores, so they also take the public
client key, not a server key. For a Node.js service that holds a server key, use
`@otto-flags/node-server`.

## Install

These packages are not installable by name yet. Install from source, pinned to a full commit SHA of
`savvagent/otto-flags`. Find the current tip of `main` with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

Ask a person to review and approve the SHA before you pin it, or use the SHA of a commit they have
already checked. It must be a commit on `main` of `savvagent/otto-flags`, not one from a fork. Use
the approved SHA in place of `<commit>`. Build and pack each package in dependency order
(`@otto-flags/sdk`, `@otto-flags/svelte`, `@otto-flags/sveltekit`), because a package's tarball
names the others it depends on and they cannot be resolved by name. You need `git`, Node.js and
`pnpm`.

On pnpm 9, `pnpm install` runs the install scripts of this repository's dependencies on your
machine; pnpm 10 and later skip them unless allowed. The lockfile fixes which versions you get, not
what their scripts do. None of these packages needs them to build, so skip them with
`--ignore-scripts`.

```bash
git clone https://github.com/savvagent/otto-flags
cd otto-flags
git checkout <commit>
pnpm install --frozen-lockfile --ignore-scripts
mkdir -p ../otto-flags-tarballs
pnpm --filter @otto-flags/sdk build
pnpm --filter @otto-flags/sdk pack --pack-destination ../otto-flags-tarballs
pnpm --filter @otto-flags/svelte build
pnpm --filter @otto-flags/svelte pack --pack-destination ../otto-flags-tarballs
pnpm --filter @otto-flags/sveltekit build
pnpm --filter @otto-flags/sveltekit pack --pack-destination ../otto-flags-tarballs
```

Then, in your own project, install all of the tarballs in one command (use the real path to the
`otto-flags-tarballs` directory):

```bash
npm install \
  /path/to/otto-flags-tarballs/otto-flags-sdk-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-svelte-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-sveltekit-*.tgz
```

The tarball file names carry the package version, hence the `*`. Another package manager's
install-from-tarball command works the same way, as long as every tarball goes in together. Pin a
SHA, not a branch, so a build is reproducible.

## Initialize

Server side: initialize once in `src/hooks.server.ts`, from the `/server` entry point.

```typescript
// src/hooks.server.ts
import { initSvelteKitServer } from "@otto-flags/sveltekit/server";

initSvelteKitServer({
  apiKey: "{{CLIENT_KEY}}",
  applicationId: "{{APP_ID}}",
  environment: "production",
});
```

Client side: initialize the stores once in the root layout.

```svelte
<!-- src/routes/+layout.svelte -->
<script>
  import { initOttoFlags } from "@otto-flags/sveltekit";

  initOttoFlags({
    config: {
      apiKey: "{{CLIENT_KEY}}",
      applicationId: "{{APP_ID}}",
    },
  });
</script>

<slot />
```

The config takes the core client's fields: `baseUrl` (default `https://otto-flags.savvagent.com`),
`environment` (default `"production"`), `enableRealtime` (default `true`), `cacheTtl` (milliseconds,
default 60000), `enableTelemetry` (default `true`), `defaults` and `onError`.

## Evaluate a flag

In a load function (server side):

```typescript
// src/routes/checkout/+page.server.ts
import { evaluateForEvent } from "@otto-flags/sveltekit/server";

export async function load(event) {
  const newCheckout = await evaluateForEvent(event, "new-checkout");
  return { newCheckout };
}
```

`evaluateForEvent` builds the context from the request: `user_id` from the `user_id` cookie,
`anonymous_id` from the `otto_flags_anonymous_id` cookie, `session_id` from the `session_id` cookie,
and `language` from `Accept-Language`. Percentage rollouts bucket on the user id, so make sure it
reaches the evaluation. To pass your own, use `isEnabled("new-checkout", { user_id: ... })`. The
other server exports are `evaluate(flagKey, context)` (full result), `withFlag(flagKey, callback,
context)`, `trackError(flagKey, error, context)`, `getEventContext(event, overrides)` and
`getServerClient()`.

In a component (client side), with the stores from `@otto-flags/svelte`:

```svelte
<script>
  import { createFlag } from "@otto-flags/sveltekit";

  const newCheckout = createFlag("new-checkout", { defaultValue: false });
</script>

{#if $newCheckout}<NewCheckout />{:else}<OldCheckout />{/if}
```

Also exported from the main entry point: `createFlagStore` (value, loading, error, result),
`createUserIdStore`, `trackErrorClient`, `getOttoFlags`, `setEnvironment` and `getEnvironment`.

## Notes

- Flag keys are 1 to 128 characters of letters, digits, `.`, `_` and `-`. A lookup with any other
  key is not sent; it returns the default and reports the problem to `onError`.
- A flag lookup never throws into the app: on a network or server error the value falls back to the
  `defaultValue` you pass (or to `defaults` in the config) and `reason` is `"error"`. Only a missing
  or malformed key makes the client constructor throw.
- Results are cached per flag and per evaluation context, so two users never share a cached result.
  With real-time updates on (the default) a change made with `set_flag_environment` or
  `rollback_flag` reaches the page within moments.
- Telemetry (evaluation counts and tracked errors) feeds `flag_health` and auto-rollback; set
  `enableTelemetry: false` in the config to turn it off.
- Create flags before code references them: an unknown flag key evaluates to its default.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
