# otto-flags Vue SDK (app: {{APP_NAME}})

`@otto-flags/vue` provides a plugin and Composition API composables for Vue 3. It wraps
`@otto-flags/sdk`, so its install includes that package.

The client key is public by design: it can ship in a browser bundle and sits in source control
without harm. It can evaluate flags but cannot read targeting rules. Do not put the app's server key
in this package: its client accepts only the public client key, and a server key must never reach a
browser.

## Install

These packages are not installable by name yet. Install from source, pinned to a full commit SHA of
`savvagent/otto-flags`. Find the current tip of `main` with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

Ask a person to review and approve the SHA before you pin it, or use the SHA of a commit they have
already checked. Use that SHA (or another one you have checked) in place of `<commit>`. Build and
pack each package in dependency order (`@otto-flags/sdk`, `@otto-flags/vue`), because a package's
tarball names the others it depends on and they cannot be resolved by name. You need `git`, Node.js
and `pnpm`.

A normal install runs your dependencies' install scripts on this machine (pnpm 10 and later skip
them unless allowed; earlier versions run them). The lockfile fixes which versions you get, not what
their scripts do. None of these packages needs them to build, so skip them with `--ignore-scripts`.

```bash
git clone https://github.com/savvagent/otto-flags
cd otto-flags
git checkout <commit>
pnpm install --frozen-lockfile --ignore-scripts
mkdir -p ../otto-flags-tarballs
pnpm --filter @otto-flags/sdk build
pnpm --filter @otto-flags/sdk pack --pack-destination ../otto-flags-tarballs
pnpm --filter @otto-flags/vue build
pnpm --filter @otto-flags/vue pack --pack-destination ../otto-flags-tarballs
```

Then, in your own project, install all of the tarballs in one command (use the real path to the
`otto-flags-tarballs` directory):

```bash
npm install \
  /path/to/otto-flags-tarballs/otto-flags-sdk-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-vue-*.tgz
```

The tarball file names carry the package version, hence the `*`. Another package manager's
install-from-tarball command works the same way, as long as every tarball goes in together. Pin a
SHA, not a branch, so a build is reproducible.

## Initialize

Install the plugin once when creating the app. It accepts the client config directly, or an object
with `config`, `defaultContext` and `initialOverrides`.

```typescript
import { createApp } from "vue";
import { OttoFlagsPlugin } from "@otto-flags/vue";
import App from "./App.vue";

createApp(App)
  .use(OttoFlagsPlugin, {
    config: {
      apiKey: "{{CLIENT_KEY}}",
      applicationId: "{{APP_ID}}",
      environment: "production",
    },
    defaultContext: { userId: currentUser?.id },
  })
  .mount("#app");
```

`config` takes the core client's fields: `baseUrl` (default `https://otto-flags.savvagent.com`),
`environment` (default `"production"`), `enableRealtime` (default `true`), `cacheTtl` (milliseconds,
default 60000), `enableTelemetry` (default `true`), `defaults` and `onError`. `defaultContext`
applies to every evaluation: `applicationId`, `environment`, `organizationId`, `userId`,
`anonymousId`, `sessionId`, `language` and `attributes`. Without the plugin, call
`provideOttoFlags(config, defaultContext)` in a root component's `setup`.

## Evaluate a flag

```vue
<script setup lang="ts">
import { useFlag } from "@otto-flags/vue";

const { value: newCheckout, loading } = useFlag("new-checkout", {
  context: { user_id: currentUser.value?.id, attributes: { plan: "pro" } },
  defaultValue: false,
});
</script>

<template>
  <p v-if="loading">Loading...</p>
  <NewCheckout v-else-if="newCheckout" />
  <OldCheckout v-else />
</template>
```

Context fields are snake_case (`user_id`, `anonymous_id`, `session_id`, `organization_id`,
`language`, `attributes`); the default-context object takes camelCase equivalents (`userId`,
`anonymousId`, `sessionId`, `organizationId`, `language`, `attributes`, plus `applicationId` and
`environment`). Percentage rollouts bucket on the user id, so pass a stable one. `useFlag` returns
refs: `value`, `loading`, `error`, `result` and a `refetch()` function. It evaluates when the
component mounts and re-evaluates when the flag changes (`realtime: true` by default).

Other composables: `useFlags(keys, { defaultValues })` for several flags at once; `useWithFlag(key,
callback)` runs a callback when a flag is on; `useUser()` gives the user id helpers (call
`setUserId(null)` on sign-out); `useTrackError(key)` returns a function that reports an error
against a flag, which is what `flag_health` and auto-rollback compare; `useEnvironment()`; and
`useOttoFlags()` returns `{ client, isReady, defaultContext }` for direct use of the core client.

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
