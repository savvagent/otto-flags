# Serve a connector install page at `/` for browsers

**Status:** DRAFT
**Issue:** #71

## 1. Brief

From #71. The ticket was filed non-interactively from a brief whose decisions were already made:

> People who click "Open console" on otto.savvagent.com land on https://otto-flags.savvagent.com/,
> which answers with the raw JSON 404 from `not_found` in `crates/flags-server/src/lib.rs`.
> otto-flags has no web console: people manage flags from an AI assistant (Claude, ChatGPT, Claude
> Code) with otto-flags added as a custom connector, and apps evaluate flags through the SDKs. A
> browser that lands on the origin should get a page that tells a person how to add the connector.
>
> **Acceptance criteria**
> - [ ] Given a browser `Accept` header, `GET /` returns 200 HTML with the headers in the ticket's Approach (§6 here) and the
>   configured connector address.
> - [ ] `HEAD /` with a browser `Accept` header returns 200 with no body.
> - [ ] `GET /` with `Accept: */*`, with `Accept: application/json`, or with an HTML `Accept` plus
>   `Authorization` returns the unchanged JSON 404.
> - [ ] An unknown path with an HTML `Accept` still returns the JSON 404.
> - [ ] An unauthenticated `/mcp` request still returns 401 with `WWW-Authenticate` naming
>   `/.well-known/oauth-protected-resource`.
> - [ ] A unit test recomputes the sha256 of the inline script and style from the embedded HTML
>   and asserts that both match the CSP.
> - [ ] All gates are green. After deploy, `curl -s -H 'Accept: text/html'
>   https://otto-flags.savvagent.com/` returns the page, and plain `curl -s
>   https://otto-flags.savvagent.com/` still returns the JSON 404.

The headers, content, and look are fixed by the brief and restated in §6.

## 2. Premise corrections

- **Adding a `GET /` route would change `POST /` from 404 to 405.** axum answers a method that a
  route does not handle with 405 Method Not Allowed, and a `get(..).fallback(..)` method router
  adds an `Allow: GET,HEAD` header even to its fallback's responses. To keep "every other request
  to `/` gets the JSON 404", `/` is routed with `any(root)`, and `root` checks for `GET`/`HEAD`
  itself. axum still strips the body for `HEAD` at the top level.
- **"Unchanged JSON 404" needs one header on `/`.** The HTML is `Cache-Control: public` and
  depends on `Accept` and `Authorization`. A cache that stored the 404 for `/` without `Vary`
  could serve it to a browser, or store the page and serve it to a JSON client. So the 404 that
  `GET`/`HEAD /` returns carries `Vary: Accept, Authorization` too. Its status and body are
  byte-for-byte unchanged, and the 404 for every other path is untouched.

## 3. Assumptions

- **What counts as a browser:** `Accept` has a media range exactly `text/html` (case-insensitive,
  parameters allowed) whose `q` is greater than 0. A missing `q` means 1. A `q` that does not
  parse, or is outside 0 to 1 (`inf`, `NaN`, `5`), counts as 0, so the request is not served
  HTML. Harmless leniency is kept: a parameter without `=` is ignored, and the last `q` wins. `text/*` and `*/*` do not count: curl and most
  HTTP libraries send `*/*`, and they must keep the JSON. Several `Accept` headers are read
  together.
- **Any `Authorization` header**, of any scheme and even an empty one, means "not a person on a
  landing page", so the request gets the JSON.
- **The connector address is `Config::resource_uri`** (production
  `https://otto-flags.savvagent.com/mcp`). It is the exact token audience, so it is what a client
  must be given. **The platform origin** is the origin of `Config::platform_url` (production
  `https://otto.savvagent.com`). The links are `<origin>/products/flags#connect` and `<origin>/`.
- **The CSP hashes are constants in the source**, not computed at startup. The brief's unit test
  recomputes them from the embedded HTML, and computing them at runtime would make that test a
  tautology. Changing the inline style or script fails the test until the constant is updated.
- **Hashing needs test-only dependencies.** `sha2` is already a workspace dependency, and `base64`
  0.22.1 is already in `Cargo.lock` through other crates. It is declared as `base64 = "0.22"`, so
  it reuses the locked 0.22.1. Both are added as `flags-server`
  dev-dependencies (base64 through `[workspace.dependencies]`), so the server binary gains no
  dependency.
- **Two copy buttons, one script:** one copies the connector address and one copies the Claude
  Code command. Each button names its target element by id (`data-copy`) and its
  status region (`data-status`). The script copies that
  element's text, so it never contains the URL and its hash stays constant. Each copyable has its
  own `role="status"` region.
- **The page is built once, in `router()`,** which runs once at startup. A test builds its own.
- Every Run Profile value was read from the Development Workflow section, so none is assumed.

## 4. Goal and success criteria

A person whose browser opens the otto-flags origin sees how to add otto-flags to their AI
assistant. Nothing else about the service changes.

- `GET /` with a browser `Accept` and no `Authorization` returns 200 `text/html; charset=utf-8`
  with the six security and caching headers below, and the configured connector address
  HTML-escaped in the body.
- `HEAD /` under the same conditions returns 200, the same headers, and an empty body.
- Every other request to `/` gets the existing JSON 404 status and body (plus `Vary`, see §2), and
  every other path is unchanged.
- The CSP allows exactly the one inline style and the one inline script, by hash. A unit test
  proves that the hashes match.

## 5. Scope

**In:** a new module `crates/flags-server/src/install_page.rs` with the `Accept` check, the page
builder, the response, and the CSP. A new file `crates/flags-server/src/install_page.html`. The
route in `router()` and the module table at the top of `crates/flags-server/src/lib.rs`. Tests in
both files. `base64` in the root `[workspace.dependencies]`, and `sha2` and `base64` as
`flags-server` dev-dependencies (with `Cargo.lock`).

**Out:** a web console. Any change to tools, auth, scopes, migrations, SDK routes, or metering.
JavaScript beyond the copy buttons. i18n. Light theme. New config keys.

## 6. Architecture

### Routing (`crates/flags-server/src/lib.rs`)

`router()` (lib.rs:51) gains, before `.fallback(not_found)`:

```rust
let page = Arc::new(InstallPage::new(&config.resource_uri, &config.platform_url));
.route("/", any(move |method: Method, headers: HeaderMap, uri: Uri| root(page.clone(), method, headers, uri)))
```

`root` returns `page.response()` when the method is `GET` or `HEAD` and
`install_page::wants_html(&headers)` holds (which includes there being no `authorization`).
Otherwise it returns `not_found(uri)` with `Vary: Accept, Authorization` added for `GET`/`HEAD`.
For other methods it returns `not_found(uri)` unchanged. axum strips the body of every top-level
`HEAD` response. `/` is
not under `/mcp` or `/api`, so the Host and Origin checks on `/mcp` and the API routers are
unaffected. axum panics at build time on a route registered twice, and the existing
`the_whole_router_assembles…` test would catch a clash.

### `crates/flags-server/src/install_page.rs`

- `pub fn wants_html(headers: &HeaderMap) -> bool` implements the rule in §3.
- `pub struct InstallPage { html: Bytes }`, so each response shares the page instead of copying it.
  `InstallPage::new(connector_url, platform_url)` replaces `{{PLATFORM_ORIGIN}}` and then
  `{{CONNECTOR_URL}}` (every occurrence) in
  `include_str!("install_page.html")` with HTML-escaped values (`&`, `<`, `>`, `"`, `'`). The
  origin comes from `url::Url::parse(platform_url)` and `.origin().ascii_serialization()` for an
  http(s) URL. For anything else it is empty, so the links become same-origin paths and no
  arbitrary scheme can reach an `href`. Config validates only `FLAGS_PLATFORM_URL` (`url_var`).
  `FLAGS_RESOURCE_URI` is trusted operator config that is escaped and shown but never parsed.
- `pub fn response(&self) -> Response` returns 200 with these headers:
  - `Content-Type: text/html; charset=utf-8`
  - `Cache-Control: public, max-age=3600`
  - `Vary: Accept, Authorization`
  - `X-Content-Type-Options: nosniff`
  - `Referrer-Policy: no-referrer`
  - `Content-Security-Policy: default-src 'none'; style-src 'sha256-<S>'; script-src
    'sha256-<J>'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`
- `const CSP: &str` holds the literal policy with the two hashes. Each hash is base64(sha256) of
  the exact bytes between `<style>` and `</style>`, and between `<script>` and `</script>`, in the
  template. Neither element contains a placeholder, so substitution cannot change either hash.

### `crates/flags-server/src/install_page.html`

The content is the brief's, verbatim:

- `<html lang="en">`, `<title>Otto Flags connector</title>`, `<meta name="viewport">`,
  `<meta name="color-scheme" content="dark">`, and exactly one `<style>`, one `<script>`, and one
  `<h1>`. There are no `style=` attributes and no inline `on…=` handlers: the CSP hashes do not
  cover them, and the hash test asserts that there are none.
- Eyebrow: "Otto Flags · connector". h1: "Use Otto Flags from your AI assistant".
- Lead: "Otto Flags doesn't have a web page here. Add it to your AI assistant as a connector, then
  ask which flags are on, roll one out, or roll one back in plain language. The first time, you
  sign in with your Otto account."
- The connector address (`<code id="connector-url">`, monospace, `user-select: all`), a Copy
  button, and a `role="status"` region. When the clipboard is refused, the script selects the text
  and says "Selected. Press Ctrl+C to copy."
- Three setups, each a heading and an ordered list:
  - Claude (claude.ai or the desktop app): Settings → Connectors → Add custom connector → paste
    the address → Connect → sign in with Otto.
  - ChatGPT: Settings → Connectors (developer mode) → Create → paste the address → sign in with
    Otto.
  - Claude Code: a copyable `claude mcp add --transport http otto-flags <address>`, then `/mcp` to
    sign in.
- Links: "Full setup guide" → `{{PLATFORM_ORIGIN}}/products/flags#connect`, and "Open your Otto
  console" → `{{PLATFORM_ORIGIN}}/`.
- Developer footnote: "Developers: the MCP endpoint is <address>. Apps evaluate flags through the
  SDKs: see /api/sdk/flags. OAuth metadata is at /.well-known/oauth-protected-resource." The two
  paths are code, not links.
- Look: `color-scheme: dark` and the palette tokens as CSS custom properties (canvas
  oklch(0.19 0.02 260), surface oklch(0.24 0.02 260), raised oklch(0.29 0.02 260), edge
  oklch(0.36 0.02 260), ink oklch(0.97 0.005 260), muted oklch(0.72 0.02 260), faint
  oklch(0.58 0.02 260), accent oklch(0.75 0.14 220), accent-ink oklch(0.2 0.05 240)). The system
  UI font stack, with monospace for the address and command. One column, `max-width: 600px`, with
  a `16px` minimum side gutter and no horizontal scroll at 360px (long addresses wrap with
  `overflow-wrap: anywhere`). `:focus-visible` gets a 2px accent outline. Body text uses ink or
  muted on canvas or surface, and faint is used only for large or non-essential text, so contrast
  stays at or above 4.5:1. The Copy button has accent-ink text on an accent background.

## 7. Invariants impact

- **MCP auth:** `/mcp` is untouched, and its unauthenticated 401 and `WWW-Authenticate` are
  re-asserted by a test. The page carries no token and makes no platform call.
- **Public interface (additive):** `GET`/`HEAD /` gain an HTML representation for browsers only,
  and the JSON for every existing client is unchanged.
- **The SDK path never calls the platform, and the SDK REST contract is stable:** no `/api` route
  changes.
- **The database is small and shared:** the page is static, built once, and needs no database or
  pool connection. A cheap unauthenticated request costs one header scan and a clone of a few KB.
- **Licensing:** the new files sit in `crates/`, which is AGPL like the rest of the crate. The
  crate's files carry no per-file headers.
- Migrations, tenancy, metering, rollout bucketing, webhooks, client keys, and the otto-platform
  pin are not touched.

## 8. Migration

None.

## 9. Out-of-band

None. There is no new config key (the change reuses `FLAGS_RESOURCE_URI` and
`FLAGS_PLATFORM_URL`), no secret, no scope, no migration, and no recurring cost.

## 10. Error handling and edge cases

- An `Accept` value that is not visible ASCII is skipped. Any other `Accept` values are still read.
- Duplicate `Accept` headers are all scanned.
- `text/html;q=0` alone means no HTML. `text/html;q=0.1` means HTML. `TEXT/HTML` means HTML.
- `Authorization` that is present but empty still means JSON.
- A connector URL containing `&` or a quote is escaped, and a test asserts it.
- The page is built without `unwrap` on request paths. Header values are static or validated
  constants, built with `HeaderValue::from_static`.
- `HEAD` gets the same headers as `GET`. axum supplies the empty body.

## 11. Testing approach

Gates are as the section lists them, with `DATABASE_URL` set to the local Postgres on 15434.

- Unit tests in `install_page.rs`, which need no database:
  - `wants_html`: browser Accept strings (Chrome, Firefox, Safari) give true. `*/*`,
    `application/json`, `text/*`, `text/html;q=0`, a junk `q`, and a missing header give false.
    Uppercase gives true. Two headers are scanned.
  - Escaping and substitution: no `{{` is left, the URL is escaped, and the origin is derived.
  - CSP hashes: recompute sha256 and base64 for the `<style>` and `<script>` bodies of the
    embedded template, and assert that `CSP` contains `'sha256-<each>'`. Assert one `<style>`, one
    `<script>`, and one `<h1>`.
- Router tests in the `lib.rs` `tests` module, which use the existing `app()` built on a lazy
  pool, so they need no database:
  - A browser Accept gives 200 with every header and the configured URL in the body.
  - `*/*` gives the JSON 404 with the exact current body.
  - `application/json` gives the JSON 404.
  - HTML Accept with `Authorization` gives the JSON 404.
  - `HEAD` with HTML Accept gives 200 and an empty body.
  - `POST /` gives the JSON 404, not 405, and carries no `Allow` header.
  - The 404 for `/` carries `Vary`, and the 404 for `/nope` does not.
  - `/nope` with HTML Accept gives the JSON 404.
  - The existing unauthenticated `/mcp` test is kept. It is extended to send `Accept: text/html` as
    well, and still expects 401 with `WWW-Authenticate`.

## 12. Risks and open questions

- The Claude and ChatGPT menu paths can drift as those products change. The page links to the
  platform's full setup guide for the current steps. Owner: Rob, who keeps the platform guide
  current.
- `/products/flags#connect` on the platform is being added by a concurrent otto-platform change.
  Until it ships, that link lands on the product page without the anchor, or on a 404 there.
  Owner: the otto-platform change.
