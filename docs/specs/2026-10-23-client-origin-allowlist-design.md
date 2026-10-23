# Let an app limit its public client key to its own websites

**Status:** IMPLEMENTED (#188)
**Ticket:** savvagent/otto-flags#143 (PR 2 of 2; PR 1, #187, added the hourly ceiling)

## Brief

Ticket #143, "Limit how far a public client key can raise an org's metered evaluations": anyone
holding an app's public client key (`sdk_`) can call the evaluate routes repeatedly, and each
answered evaluation is billed. The decision made with the owner (Rob) on 2026-10-23 was to bound
the exposure rather than remove it. PR 1 caps the billed quantity per app and hour. This PR is the
second measure: an origin allowlist, so a page on another website cannot use an app's public key
from a visitor's browser.

## Premise corrections

- CORS does not do this. The SDK routes answer CORS for any origin (`flags-api/src/lib.rs`), by
  design: the preflight carries no key, so it cannot know which app's list applies. The check
  therefore happens on the real request, after the key is resolved, and the preflight is unchanged.
- The `Origin` header is not authentication. Browsers send it honestly on cross-origin calls; a
  script can send any value or none. This stops other sites from using a key, not an attacker with
  a script, which is what the ceiling, `usage` split and key rotation are for.

## Decisions

- **Storage.** `flag_apps.allowed_origins text[] NOT NULL DEFAULT '{}'` (migration 0011), at most
  20 entries (also a CHECK). Empty means unrestricted, which is what every app has today, so no
  existing app changes behavior and the previous release, which never reads the column, still boots.
- **Entry shape** (`flags_core::apps::normalize_origins`): `http(s)://host[:port]`, lowercased,
  trimmed, deduplicated, order kept; host labels are letters, digits and `-`; no path, query,
  userinfo or trailing slash; at most 253 characters. A leading `*.` (`https://*.example.com`)
  matches any subdomain of `example.com`, at any depth, with the same scheme and port, but not
  `example.com` itself. No other wildcard.
- **Match** (`origin_allowed`): an empty list allows all; otherwise the request's origin must equal
  an entry or match a wildcard entry. The literal `null` origin and any value that is not an origin
  match nothing.
- **Enforcement.** Only a client-key request that carries an `Origin` header, for an app with a
  non-empty list, and only when the origin is not allowed, is refused: `403 origin_not_allowed`.
  The body names neither the list nor the app. A request with no `Origin` (native apps, servers,
  `curl`) and every server-key request pass. Routes: `POST /api/evaluate/{key}`,
  `POST /api/flags/{key}/evaluate` (the billed ones) and `GET /api/sdk/flags`. A refused
  evaluation is not counted.
- **No new round trips.** Both handlers already read the app inside the transaction they open
  (`evaluate` since PR 1, for the environment check); the list rides on that read.
- **Management.** `update_app` takes an optional `allowedOrigins` and a now-optional
  `environments` (at least one must be sent). It needs the `apps:admin` scope and the fresh
  owner/admin lookup, like every app change. Setting the list it already has changes nothing and
  writes no audit entry; a change is audited under `flags.app.updated` with `from`/`to`. `list_apps`
  and every app result show `allowedOrigins`. `create_app` is unchanged: a new app is unrestricted
  until an admin sets a list.

## Non-goals

- The flag-change stream (`/api/flags/stream`) and the telemetry routes are not checked. The
  stream is not billed and telemetry from a client key is not billed (PR 1 and #146); the check
  can be added there if they ever become billing paths.
- Per-app ceiling overrides (listed in PR 1) and short-lived client tokens.
- Any change to CORS headers or to the SDKs.

## Risks

- A site that proxies the SDK call from its own server sends no `Origin` and passes: expected,
  and the same as a script.
- An app that sets a list but forgets a real origin (a preview deploy, `localhost`) gets 403 in
  that page until an admin adds it. The SDKs treat a 403 like any non-200 and serve defaults and
  cached values, and the error text says what to do.

## Success criteria

- An app with no list behaves exactly as before.
- With a list, a client key from an allowed origin evaluates and is counted; from another origin it
  gets 403 and is not counted; with no `Origin`, or as a server key, it passes.
- Only an owner or admin can set the list; bad entries are refused without changing the list.
