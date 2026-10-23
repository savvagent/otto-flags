# otto-flags React SDK (app: {{APP_NAME}})

`@otto-flags/react` provides a provider and hooks for React 16.8 or later. It wraps
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
pack each package in dependency order (`@otto-flags/sdk`, `@otto-flags/react`), because a package's
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
pnpm --filter @otto-flags/react build
pnpm --filter @otto-flags/react pack --pack-destination ../otto-flags-tarballs
```

Then, in your own project, install all of the tarballs in one command (use the real path to the
`otto-flags-tarballs` directory):

```bash
npm install \
  /path/to/otto-flags-tarballs/otto-flags-sdk-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-react-*.tgz
```

The tarball file names carry the package version, hence the `*`. Another package manager's
install-from-tarball command works the same way, as long as every tarball goes in together. Pin a
SHA, not a branch, so a build is reproducible.

## Initialize

Wrap the app once, near the root. The provider creates the client, closes it on unmount and
re-creates it if `apiKey`, `baseUrl` or the default `userId` changes.

```tsx
import { OttoFlagsProvider } from "@otto-flags/react";

const config = {
  apiKey: "{{CLIENT_KEY}}",
  applicationId: "{{APP_ID}}",
  environment: "production",
};

export function Root() {
  const defaultContext = { userId: currentUser?.id };
  return (
    <OttoFlagsProvider config={config} defaultContext={defaultContext}>
      <App />
    </OttoFlagsProvider>
  );
}
```

Define `config` outside the component (or memoize it) so it keeps one identity. `config` takes the
core client's fields: `baseUrl` (default `https://otto-flags.savvagent.com`), `environment` (default
`"production"`), `enableRealtime` (default `true`), `cacheTtl` (milliseconds, default 60000),
`enableTelemetry` (default `true`), `defaults` and `onError`. `defaultContext` applies to every
evaluation: `applicationId`, `environment`, `organizationId`, `userId`, `anonymousId`, `sessionId`,
`language` and `attributes`. `initialOverrides` (a flag key to boolean map) applies local overrides
on mount.

## Evaluate a flag

```tsx
import { useFlag } from "@otto-flags/react";

export function Checkout() {
  const { value, loading, error } = useFlag("new-checkout", {
    context: { user_id: currentUser?.id, attributes: { plan: "pro" } },
    defaultValue: false,
  });

  if (loading) return <Spinner />;
  return value ? <NewCheckout /> : <OldCheckout />;
}
```

Context fields are snake_case (`user_id`, `anonymous_id`, `session_id`, `organization_id`,
`language`, `attributes`); the default-context object takes camelCase equivalents (`userId`,
`anonymousId`, `sessionId`, `organizationId`, `language`, `attributes`, plus `applicationId` and
`environment`). Percentage rollouts bucket on the user id, so pass a stable one. `useFlag` also
returns `result` (the full evaluation) and `refetch()`; it re-evaluates when the flag changes
(`realtime: true` by default).

Other hooks: `useFlags(keys, { defaultValues })` returns `values`, `errors`, `results` and `loading`
for several flags at once; `useWithFlag(key, callback)` runs a callback when a flag is on;
`useUser()` gives `userId`, `setUserId` (call `setUserId(null)` on sign-out) and the anonymous id
helpers; `useTrackError(key)` returns a function that reports an error against a flag, which is what
`flag_health` and auto-rollback compare; `useEnvironment()` reads and sets the environment;
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
