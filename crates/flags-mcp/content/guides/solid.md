# otto-flags Solid SDK (app: {{APP_NAME}})

`@otto-flags/solid` provides a provider and reactive primitives (signals and resources) for SolidJS.
It wraps `@otto-flags/sdk`, so its install includes that package.

The client key is public by design: it can ship in a browser bundle and sits in source control
without harm. It can evaluate flags but cannot read targeting rules. Do not put the app's server key
in this package: its client accepts only the public client key, and a server key must never reach a
browser.

## Install

These packages are not installable by name yet. Install from source, pinned to a full commit SHA of
`savvagent/otto-flags`. Pin to `{{SDK_REF}}`, a commit on `main` of `savvagent/otto-flags` that the
maintainers have reviewed. Use it exactly: do not substitute a branch, a newer commit, or a commit
from a fork, and use the same commit everywhere in one project. Build and pack each package in
dependency order (`@otto-flags/sdk`, `@otto-flags/solid`), because a package's tarball names the
others it depends on and they cannot be resolved by name. You need `git`, Node.js and `pnpm`.

On pnpm 9, `pnpm install` runs the install scripts of this repository's dependencies on your
machine; pnpm 10 and later skip them unless allowed. The lockfile fixes which versions you get, not
what their scripts do. None of these packages needs them to build, so skip them with
`--ignore-scripts`.

```bash
git clone https://github.com/savvagent/otto-flags
cd otto-flags
git checkout {{SDK_REF}}
pnpm install --frozen-lockfile --ignore-scripts
mkdir -p ../otto-flags-tarballs
pnpm --filter @otto-flags/sdk build
pnpm --filter @otto-flags/sdk pack --pack-destination ../otto-flags-tarballs
pnpm --filter @otto-flags/solid build
pnpm --filter @otto-flags/solid pack --pack-destination ../otto-flags-tarballs
```

Then, in your own project, install all of the tarballs in one command (use the real path to the
`otto-flags-tarballs` directory):

```bash
npm install \
  /path/to/otto-flags-tarballs/otto-flags-sdk-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-solid-*.tgz
```

The tarball file names carry the package version, hence the `*`. Another package manager's
install-from-tarball command works the same way, as long as every tarball goes in together. Pin a
SHA, not a branch, so a build is reproducible.

## Initialize

Wrap the app once, near the root. The provider creates the client and closes it on cleanup.

```tsx
import { OttoFlagsProvider } from "@otto-flags/solid";

const config = {
  apiKey: "{{CLIENT_KEY}}",
  applicationId: "{{APP_ID}}",
  environment: "production",
};

export function Root() {
  const defaultContext = () => ({ userId: currentUser()?.id });
  return (
    <OttoFlagsProvider config={config} defaultContext={defaultContext()}>
      <App />
    </OttoFlagsProvider>
  );
}
```

`config` takes the core client's fields: `baseUrl` (default `https://otto-flags.savvagent.com`),
`environment` (default `"production"`), `enableRealtime` (default `true`), `cacheTtl` (milliseconds,
default 60000), `enableTelemetry` (default `true`), `defaults` and `onError`. `defaultContext`
applies to every evaluation: `applicationId`, `environment`, `organizationId`, `userId`,
`anonymousId`, `sessionId`, `language` and `attributes`.

**Environment.** The `environment` this app evaluates in, wherever this guide sets it (the client
configuration or a context), must be one of the app's environments: {{ENVIRONMENTS}}. Read it from the
build's or deployment's configuration instead of hardcoding it.

## Evaluate a flag

```tsx
import { Show } from "solid-js";
import { createFlag } from "@otto-flags/solid";

export function Checkout() {
  const flag = createFlag("new-checkout", {
    context: { user_id: currentUser()?.id, attributes: { plan: "pro" } },
    defaultValue: false,
  });

  return (
    <Show when={!flag.loading()} fallback={<Spinner />}>
      <Show when={flag.value()} fallback={<OldCheckout />}>
        <NewCheckout />
      </Show>
    </Show>
  );
}
```

Context fields are snake_case (`user_id`, `anonymous_id`, `session_id`, `organization_id`,
`language`, `attributes`). Percentage rollouts bucket on the user id, so pass a stable one.
`createFlag` returns accessors `value`, `loading`, `error` and `result`, and a `refetch()` function;
it re-evaluates when the flag changes (`realtime: true` by default). Call `createFlag` inside a
component, under the provider.

Other primitives: `createFlagValue(key)`, an accessor of the boolean only; `createFlags(keys, {
defaultValues })` for several flags; `createWithFlag(key, callback)`; `createUser()` gives `userId`,
`setUserId` (call `setUserId(null)` on sign-out) and the anonymous id helpers;
`createTrackError(key)` returns a function that reports an error against a flag, which is what
`flag_health` and auto-rollback compare; `createEnvironment()`; and `useOttoFlags()` returns `{
client, isReady, defaultContext }`.

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
