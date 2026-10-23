# otto-flags Next.js SDK (app: {{APP_NAME}})

`@otto-flags/nextjs` covers the App Router: Server Components, Client Components, middleware, route
handlers and Server Actions, through separate entry points. It wraps `@otto-flags/sdk` and
`@otto-flags/react`; its install includes both.

The client key is public by design: it can ship in a browser bundle and sits in source control
without harm. It can evaluate flags but cannot read targeting rules. Do not put the app's server key
in this package: its client accepts only the public client key, and a server key must never reach a
browser.

The server and middleware helpers use the same core client as the browser, so they also take the
public client key, not a server key. For a Node.js service that holds a server key, use
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
(`@otto-flags/sdk`, `@otto-flags/react`, `@otto-flags/nextjs`), because a package's tarball names
the others it depends on and they cannot be resolved by name. You need `git`, Node.js and `pnpm`.

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
pnpm --filter @otto-flags/react build
pnpm --filter @otto-flags/react pack --pack-destination ../otto-flags-tarballs
pnpm --filter @otto-flags/nextjs build
pnpm --filter @otto-flags/nextjs pack --pack-destination ../otto-flags-tarballs
```

Then, in your own project, install all of the tarballs in one command (use the real path to the
`otto-flags-tarballs` directory):

```bash
npm install \
  /path/to/otto-flags-tarballs/otto-flags-sdk-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-react-*.tgz \
  /path/to/otto-flags-tarballs/otto-flags-nextjs-*.tgz
```

The tarball file names carry the package version, hence the `*`. Another package manager's
install-from-tarball command works the same way, as long as every tarball goes in together. Pin a
SHA, not a branch, so a build is reproducible.

## Initialize

Server side: initialize once, from the `/server` entry point, in a module the root layout loads.

```tsx
// app/layout.tsx
import { initServerClient } from "@otto-flags/nextjs/server";

initServerClient({
  apiKey: "{{CLIENT_KEY}}",
  applicationId: "{{APP_ID}}",
  environment: "production",
});

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
```

Client side: a provider component marked `'use client'`, from the `/client` entry point.

```tsx
// app/providers.tsx
"use client";

import { OttoFlagsProvider } from "@otto-flags/nextjs/client";

const config = { apiKey: "{{CLIENT_KEY}}", applicationId: "{{APP_ID}}" };

export function Providers({ children }: { children: React.ReactNode }) {
  return <OttoFlagsProvider config={config}>{children}</OttoFlagsProvider>;
}
```

Render `<Providers>` inside `<body>` in the root layout. Middleware has its own client:
`initMiddlewareClient(config)` from `@otto-flags/nextjs/middleware`. The config takes the core
client's fields: `baseUrl` (default `https://otto-flags.savvagent.com`), `environment` (default
`"production"`), `enableRealtime` (default `true`), `cacheTtl` (milliseconds, default 60000),
`enableTelemetry` (default `true`), `defaults` and `onError`.

## Evaluate a flag

In a Server Component:

```tsx
// app/page.tsx
import { createServerContext, isEnabled } from "@otto-flags/nextjs/server";

export default async function Page() {
  const context = await createServerContext({ attributes: { plan: "pro" } });
  const enabled = await isEnabled("new-layout", context);
  return enabled ? <NewLayout /> : <OldLayout />;
}
```

`createServerContext` reads the request's cookies and headers to build the context. Percentage
rollouts bucket on `user_id`, so make sure one reaches the evaluation. Context fields are snake_case
(`user_id`, `anonymous_id`, `session_id`, `organization_id`, `language`, `attributes`). The other
server exports are `evaluate(flagKey, context)` (full result), `withFlag(flagKey, callback,
context)`, `trackError(flagKey, error, context)`, `evaluateForRequest(request, flagKey, context)`
for route handlers, `evaluateMultiple` and `isEnabledMultiple` for several flags at once, and
`getServerClient()`. They work in Server Actions too.

In a Client Component:

```tsx
"use client";

import { useFlag } from "@otto-flags/nextjs/client";

export function Feature() {
  const { value, loading } = useFlag("new-feature", { defaultValue: false });
  if (loading) return <Spinner />;
  return value ? <NewFeature /> : <OldFeature />;
}
```

The client entry point re-exports the hooks of `@otto-flags/react`: `OttoFlagsProvider`, `useFlag`,
`useFlags`, `useWithFlag`, `useUser`, `useTrackError`, `useEnvironment` and `useOttoFlags`.

In middleware:

```typescript
// middleware.ts
import { NextResponse } from "next/server";
import { createMiddleware, initMiddlewareClient } from "@otto-flags/nextjs/middleware";

initMiddlewareClient({ apiKey: "{{CLIENT_KEY}}", applicationId: "{{APP_ID}}" });

export default createMiddleware({
  async onRequest(request, client) {
    const user_id = request.cookies.get("user_id")?.value;
    if (await client.isEnabled("maintenance-mode", { user_id })) {
      return NextResponse.rewrite(new URL("/maintenance", request.url));
    }
  },
});
```

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
