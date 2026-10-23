# otto-flags TypeScript and JavaScript SDK (app: {{APP_NAME}})

`@otto-flags/sdk` is the core client for browsers and other JavaScript runtimes: it evaluates flags
from an app's frontend or any code that can hold the public client key, caches results, follows flag
changes in real time and reports errors from flagged code. The framework packages
(`@otto-flags/react`, `@otto-flags/vue`, and the rest) are built on it; use one of those when the
app uses that framework. For a Node.js backend that holds the secret server key, use
`@otto-flags/node-server` instead. The client key is public by design: it can ship in a browser
bundle and sits in source control without harm. It can evaluate flags but cannot read targeting
rules. Do not put the app's server key in this package: its client accepts only the public client
key, and a server key must never reach a browser.

## Install

These packages are not installable by name yet. Install from source, pinned to a full commit SHA of
`savvagent/otto-flags`. Pin to `{{SDK_REF}}`, a commit on `main` of `savvagent/otto-flags` that the
maintainers have reviewed. Use it exactly: do not substitute a branch, a newer commit, or a commit
from a fork, and use the same commit everywhere in one project. Build and pack each package in
dependency order (`@otto-flags/sdk`), because a package's tarball names the others it depends on and
they cannot be resolved by name. You need `git`, Node.js and `pnpm`.

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
```

Then, in your own project, install all of the tarballs in one command (use the real path to the
`otto-flags-tarballs` directory):

```bash
npm install \
  /path/to/otto-flags-tarballs/otto-flags-sdk-*.tgz
```

The tarball file names carry the package version, hence the `*`. Another package manager's
install-from-tarball command works the same way, as long as every tarball goes in together. Pin a
SHA, not a branch, so a build is reproducible.

## Initialize

Create one client per page or process and reuse it; it owns the cache, the telemetry queue and the
real-time connection.

```typescript
import { FlagClient } from "@otto-flags/sdk";

const client = new FlagClient({
  apiKey: "{{CLIENT_KEY}}",
  applicationId: "{{APP_ID}}",
  environment: "production",
  defaults: { "new-checkout": false },
});

// ... when the page or process shuts down:
client.close();
```

The constructor throws if `apiKey` is missing or is not a public client key, so a misconfigured
build fails at start-up. Other `FlagClientConfig` fields, all optional: `baseUrl` (default
`https://otto-flags.savvagent.com`), `environment` (default `"production"`), `enableRealtime`
(default `true`), `cacheTtl` (milliseconds, default 60000), `enableTelemetry` (default `true`),
`defaults` (flag key to the value served when evaluation fails), `onError` (receives SDK errors; the
default logs to the console), `defaultLanguage`, `disableLanguageDetection`, `retryAttempts`
(default 3), `retryDelay` (milliseconds, default 1000) and `retryBackoff` (`"linear"` or
`"exponential"`, the default).

**Environment.** The `environment` this app evaluates in, wherever this guide sets it (the client
configuration or a context), must be one of the app's environments: {{ENVIRONMENTS}}. Read it from the
build's or deployment's configuration instead of hardcoding it.

## Evaluate a flag

Pass a context describing who the evaluation is for. Percentage rollouts bucket on `user_id` (or
`anonymous_id`, then `session_id`), so pass a stable identifier. Context fields are snake_case:
`user_id`, `anonymous_id`, `session_id`, `environment`, `organization_id`, `application_id`,
`language` and `attributes`.

```typescript
const ctx = { user_id: "user-123", attributes: { plan: "pro" } };

if (await client.isEnabled("new-checkout", ctx)) {
  renderNewCheckout();
} else {
  renderOldCheckout();
}

const result = await client.evaluate("new-checkout", ctx);
// result.value (boolean), result.reason ("evaluated" | "cached" | "default" | "error"),
// result.metadata?.configuration, result.metadata?.variation
```

If you always evaluate for the same person, set the identity once and omit it from each call:
`client.setUserId("user-123")` (and `client.setUserId(null)` on sign-out); an anonymous visitor gets
a generated id from `client.getAnonymousId()`.

Run code only when a flag is on, with errors reported against the flag:

```typescript
const data = await client.withFlag("new-algorithm", async () => runNewAlgorithm(), ctx);
// data is null when the flag is off; an error thrown inside is reported, then re-thrown
```

`client.trackError(flagKey, error, ctx)` reports an error you caught yourself, which is what
`flag_health` and auto-rollback compare. `client.subscribe(flagKey, callback)` (or `"*"` for every
flag) runs the callback when a flag changes and returns an unsubscribe function.

Results are cached per flag and per evaluation context for `cacheTtl`, so two users never share a
cached result. `client.clearCache()` clears it by hand.

## Notes

- A flag lookup never throws into the app: on a network or server error the SDK returns the value
  from `defaults` (or `false`) with `reason: "error"` and passes the error to `onError`.
- Flag keys are 1 to 128 characters of letters, digits, `.`, `_` and `-`. A lookup with any other
  key is not sent; it returns the default and reports the problem to `onError`.
- Telemetry (evaluation counts and tracked errors) is batched and sent in the background; it feeds
  `flag_health` and auto-rollback. Set `enableTelemetry: false` to turn it off.
- With `enableRealtime` on, the client keeps a Server-Sent Events connection and drops its cached
  value when a flag changes, so a change made with `set_flag_environment` or `rollback_flag` reaches
  the page within moments rather than after `cacheTtl`.
- Create flags before code references them: an unknown flag key evaluates to its default.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
