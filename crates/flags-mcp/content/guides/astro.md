# otto-flags Astro SDK (app: {{APP_NAME}})

`@otto-flags/astro` is an Astro integration with server-side helpers and middleware. It wraps
`@otto-flags/sdk`, so its install includes that package.

The client key is public by design: it can ship in a browser bundle and sits in source control
without harm. It can evaluate flags but cannot read targeting rules. Do not put the app's server key
in this package: its client accepts only the public client key, and a server key must never reach a
browser.

The server helpers use the same core client as the browser packages, so they also take the public
client key, not a server key. For a Node.js service that holds a server key, use
`@otto-flags/node-server`.

## Install

These packages are not installable by name yet. Install from source, pinned to a full commit SHA of
`savvagent/otto-flags`. Find the current tip of `main` with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

and use that SHA (or another one you have checked) in place of `<commit>`. Build and pack each
package in dependency order (`@otto-flags/sdk`, `@otto-flags/astro`), because a package's tarball
names the others it depends on and they cannot be resolved by name. You need `git`, Node.js and
`pnpm`.

```bash
git clone https://github.com/savvagent/otto-flags
cd otto-flags
git checkout <commit>
pnpm install --frozen-lockfile
mkdir -p ../otto-flags-tarballs
pnpm --filter @otto-flags/sdk build
pnpm --filter @otto-flags/sdk pack --pack-destination ../otto-flags-tarballs
pnpm --filter @otto-flags/astro build
pnpm --filter @otto-flags/astro pack --pack-destination ../otto-flags-tarballs
```

Then, in your own project, install all of the tarballs in one command (use the real path to the
`otto-flags-tarballs` directory):

```bash
npm install \
  /path/to/otto-flags-tarballs/otto-flags-sdk-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-astro-*.tgz
```

The tarball file names carry the package version, hence the `*`. Another package manager's
install-from-tarball command works the same way, as long as every tarball goes in together. Pin a
SHA, not a branch, so a build is reproducible.

## Initialize

Add the integration to the Astro config:

```typescript
// astro.config.mjs
import { defineConfig } from "astro/config";
import ottoFlags from "@otto-flags/astro";

export default defineConfig({
  integrations: [
    ottoFlags({
      config: {
        apiKey: "{{CLIENT_KEY}}",
        applicationId: "{{APP_ID}}",
        environment: "production",
      },
    }),
  ],
});
```

The integration creates the client while Astro loads its configuration. For pages rendered on
request, also initialize the client in the module that serves them, so it exists in the server
process; `initOttoFlags` creates it once and ignores repeat calls:

```typescript
// src/middleware.ts
import { initOttoFlags } from "@otto-flags/astro";

initOttoFlags({ apiKey: "{{CLIENT_KEY}}", applicationId: "{{APP_ID}}" });
```

The config takes the core client's fields: `baseUrl` (default `https://otto-flags.savvagent.com`),
`environment` (default `"production"`), `enableRealtime` (default `true`), `cacheTtl` (milliseconds,
default 60000), `enableTelemetry` (default `true`), `defaults` and `onError`.

## Evaluate a flag

In an Astro component, on the server:

```astro
---
import { evaluateForRequest } from "@otto-flags/astro";

const newLayout = await evaluateForRequest(Astro.request, "new-layout");
---

{newLayout ? <NewLayout /> : <OldLayout />}
```

`evaluateForRequest` builds the context from the request (user, anonymous and session ids from
cookies, language from `Accept-Language`). Percentage rollouts bucket on the user id, so make sure
it reaches the evaluation. To pass your own context use `isEnabled("new-layout", { user_id: ... })`.
The other server exports are `evaluate(flagKey, context)` (full result), `withFlag(flagKey,
callback, context)`, `trackError(flagKey, error, context)`, `getRequestContext(request, overrides)`,
`setEnvironment`, `getEnvironment` and `getOttoFlags()`.

Route on a flag in middleware:

```typescript
// src/middleware.ts (the same file as the initialization above)
import { sequence } from "astro/middleware";
import { createFlagMiddleware, initOttoFlags } from "@otto-flags/astro";

initOttoFlags({ apiKey: "{{CLIENT_KEY}}", applicationId: "{{APP_ID}}" });

export const onRequest = sequence(
  createFlagMiddleware({
    "maintenance-mode": { redirect: "/maintenance" },
  }),
);
```

For interactive islands, use the matching framework package inside the island (`@otto-flags/react`,
`@otto-flags/vue`, `@otto-flags/svelte` or `@otto-flags/solid`) with the same public client key.

## Notes

- Flag keys are 1 to 128 characters of letters, digits, `.`, `_` and `-`. A lookup with any other
  key is not sent; it returns the default and reports the problem to `onError`.
- A flag lookup never throws into the app: on a network or server error the value falls back to the
  `defaults` in the config and `reason` is `"error"`. Only a missing
  or malformed key makes the client constructor throw.
- Results are cached per flag and per evaluation context, so two users never share a cached result.
  With real-time updates on (the default) a change made with `set_flag_environment` or
  `rollback_flag` reaches the page within moments.
- Telemetry (evaluation counts and tracked errors) feeds `flag_health` and auto-rollback; set
  `enableTelemetry: false` in the config to turn it off.
- Create flags before code references them: an unknown flag key evaluates to its default.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
