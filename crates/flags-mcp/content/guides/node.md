# otto-flags Node.js server SDK (app: {{APP_NAME}})

`@otto-flags/node-server` is the server-side Node.js client: it evaluates flags for a backend
service. It authenticates with the app's server key. That key is shown once, by `create_app` or
`rotate_app_keys`; keep it in the app's secret store and read it from the `OTTO_FLAGS_API_KEY`
environment variable. Never write it into code, a config file under version control, or a browser
bundle. Node.js 16 or later. For code that runs in a browser use `@otto-flags/sdk` (or a framework
package) with the public client key instead.

## Install

These packages are not installable by name yet. Install from source, pinned to a full commit SHA of
`savvagent/otto-flags`. Find the current tip of `main` with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

and use that SHA (or another one you have checked) in place of `<commit>`. Build and pack each
package in dependency order (`@otto-flags/sdk`, `@otto-flags/node-server`), because a package's
tarball names the others it depends on and they cannot be resolved by name. You need `git`, Node.js
and `pnpm`.

```bash
git clone https://github.com/savvagent/otto-flags
cd otto-flags
git checkout <commit>
pnpm install --frozen-lockfile
mkdir -p ../otto-flags-tarballs
pnpm --filter @otto-flags/sdk build
pnpm --filter @otto-flags/sdk pack --pack-destination ../otto-flags-tarballs
pnpm --filter @otto-flags/node-server build
pnpm --filter @otto-flags/node-server pack --pack-destination ../otto-flags-tarballs
```

Then, in your own project, install all of the tarballs in one command (use the real path to the
`otto-flags-tarballs` directory):

```bash
npm install \
  /path/to/otto-flags-tarballs/otto-flags-sdk-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-node-server-*.tgz
```

The tarball file names carry the package version, hence the `*`. Another package manager's
install-from-tarball command works the same way, as long as every tarball goes in together. Pin a
SHA, not a branch, so a build is reproducible.

## Initialize

Create one client per process and reuse it; it owns the cache, the telemetry queue and the real-time
connection.

```typescript
import { FlagClient } from "@otto-flags/node-server";

const apiKey = process.env.OTTO_FLAGS_API_KEY;
if (!apiKey) throw new Error("OTTO_FLAGS_API_KEY is not set");

export const client = new FlagClient({
  apiKey,
  applicationId: "{{APP_ID}}",
  environment: "production",
  defaults: { "new-checkout": false },
});

// ... on shutdown:
process.on("SIGTERM", () => client.close());
```

The constructor throws if the key is missing or malformed, so a misconfigured deploy fails at
start-up, not per request. Other `FlagClientConfig` fields, all optional: `baseUrl` (default
`https://otto-flags.savvagent.com`), `environment` (default `"production"`), `enableRealtime`
(default `true`), `cacheTtl` (milliseconds, default 60000), `enableTelemetry` (default `true`),
`timeout` (request timeout in milliseconds, default 5000), `defaults` (flag key to the value served
when evaluation fails) and `onError` (receives SDK errors).

## Evaluate a flag

Pass a context describing who the evaluation is for. Percentage rollouts bucket on `user_id` (or
`anonymous_id`, then `session_id`), so pass a stable identifier. Context fields are snake_case:
`user_id`, `anonymous_id`, `session_id`, `environment`, `organization_id`, `application_id`,
`language`, `attributes`, and, server-side only, `ip_address` and `user_agent`.

```typescript
const ctx = { user_id: "user-123", attributes: { plan: "pro" } };

if (await client.isEnabled("new-checkout", ctx)) {
  renderNewCheckout();
} else {
  renderOldCheckout();
}

const result = await client.evaluate("new-checkout", ctx);
// result.value (boolean), result.variation, result.configuration, result.reason
```

Also available: `getConfig(flagKey, context, defaultValue)` for configuration attached to a flag
(`defaultValue`, or null, when the flag is off), `getVariation(flagKey, context)` for multi-variant
flags (`{ variation, enabled, configuration }`), and `trackError(flagKey, error, context)` to report
an exception raised in flagged code, which is what `flag_health` and auto-rollback compare.
`subscribe(flagKey, callback)` runs the callback when a flag changes and returns an unsubscribe
function; `invalidateCache(flagKey?)` clears cached results.

## Frameworks

### Express

Build the context from the request, once per request:

```typescript
import express from "express";
import { client } from "./flags";

const app = express();

app.get("/checkout", async (req, res) => {
  const useNew = await client.isEnabled("new-checkout", {
    user_id: req.header("X-User-ID"),
    ip_address: req.ip,
    user_agent: req.header("user-agent"),
  });
  res.json({ newCheckout: useNew });
});
```

## Notes

- A flag lookup never throws into the app: on a network or server error the SDK returns the value
  from `defaults` (or `false`) with `reason: "error"` and passes the error to `onError`.
- Flag keys are 1 to 128 characters of letters, digits, `.`, `_` and `-`. A lookup with any other
  key is not sent; it returns the default and reports the problem to `onError`.
- Results are cached per flag and per evaluation context for `cacheTtl`, so two users never share a
  cached result.
- Telemetry (evaluation counts and tracked errors) is batched and sent in the background; it feeds
  `flag_health` and auto-rollback. Set `enableTelemetry: false` to turn it off.
- With `enableRealtime` on, the client keeps a Server-Sent Events connection and drops its cached
  value when a flag changes, so a change made with `set_flag_environment` or `rollback_flag` reaches
  the service within moments rather than after `cacheTtl`.
- Create flags before code references them: an unknown flag key evaluates to its default.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
