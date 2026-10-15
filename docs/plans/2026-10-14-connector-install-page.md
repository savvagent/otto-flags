# Connector Install Page Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [x]`) syntax for tracking.

**Goal:** Serve a static install page at `GET`/`HEAD /` to browsers without `Authorization`.
Every other request keeps the JSON 404.

**Architecture:** A new `install_page` module in `flags-server` owns the `Accept` rule, the
embedded HTML template, substituting the address and origin once at startup, and the response
with a hash-based CSP. `router()` adds a `/` route through `any(...)`, and its `root` handler checks for
`GET`/`HEAD` and falls back to the existing `not_found`.

**Tech Stack:** Rust, axum 0.8, http 1, url 2. sha2 0.10 and base64 0.22 for tests only.

**Ticket:** savvagent/otto-flags#71

**Spec:** `docs/specs/2026-10-14-connector-install-page-design.md`

**Shipped in #75.** The code embedded below is the plan as approved. Review commits on #75 changed
the template's aria attributes, the `q` range, the link-origin fallback, and the test-only
dependencies, so the code in `crates/flags-server/src/install_page.*` is the source of truth.

## Global Constraints

- Gates, run from the worktree `/home/robhicks/dev/otto-flags/.claude/worktrees/feat-71-connector-install-page`:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    must print nothing
  - when `crates/**` or `Cargo.toml`/`Cargo.lock` change: `podman build -t otto-flags:check .`
- Evidence: report pass counts per test binary. `cargo test` without `DATABASE_URL` panics. That
  panic is a missing variable, not a test failure. Skipped tests are not a pass. `DATABASE_URL`
  points only at the local Postgres on 15434.
- Invariants that apply: MCP auth is unchanged (`/mcp` 401 + `WWW-Authenticate`). The SDK REST
  contract is unchanged. No database is used on the page path. Code in `crates/` is AGPL. No SQL
  outside `flags-core`, and none is added.
- Worktree only. Trunk changes only through the PR.
- Before merge, a final docs commit in the same PR sets the spec's `**Status:**` to
  `IMPLEMENTED (#<PR>)` and ticks this plan's boxes. Intermediate commits end `Refs #71`, and the
  final feature commit ends `Fixes #71`.
- Commit format: `<type>(<scope>): <imperative, lower-case>`. The body explains why and ends
  `Fixes #71`. No attribution of any kind: no Co-Authored-By, no "Generated with", no credit
  markers.

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `crates/flags-server/src/install_page.html` | The page template, with `{{CONNECTOR_URL}}` and `{{PLATFORM_ORIGIN}}` placeholders |
| **Create.** `crates/flags-server/src/install_page.rs` | `wants_html`, `InstallPage`, `CSP`, `VARY`, escaping, and unit tests |
| **Modify.** `crates/flags-server/src/lib.rs` | `pub mod install_page;`, the `/` route plus the `root` handler, the module doc table, and router tests |
| **Modify.** `Cargo.toml` | `base64 = "0.22"` in `[workspace.dependencies]` |
| **Modify.** `crates/flags-server/Cargo.toml` | dev-dependencies `sha2` and `base64` |
| **Modify.** `Cargo.lock` | updated by cargo |

## Task Order & Rationale

Task 1 builds the module and proves it in isolation: the `Accept` rule, substitution, escaping,
and the CSP hashes. Task 2 wires it into the router and proves the routing acceptance criteria end
to end. Task 2 depends on Task 1's public items (`InstallPage::new`, `InstallPage::response`,
`wants_html`, `VARY`).

---

### Task 1: The install page module

**Files:**
- Create: `crates/flags-server/src/install_page.html`
- Create: `crates/flags-server/src/install_page.rs`
- Modify: `crates/flags-server/src/lib.rs` (add `pub mod install_page;` after `pub mod health;`)
- Modify: `Cargo.toml` (`[workspace.dependencies]`: add `base64 = "0.22"` after `sha2 = "0.10"`)
- Modify: `crates/flags-server/Cargo.toml` (`[dev-dependencies]`: `sha2.workspace = true`,
  `base64.workspace = true`)
- Test: unit tests inside `crates/flags-server/src/install_page.rs`

**Interfaces:** produces `install_page::{InstallPage, wants_html, CSP, VARY}`:
`InstallPage::new(connector_url: &str, platform_url: &str) -> InstallPage`,
`InstallPage::html(&self) -> &str`, `InstallPage::response(&self) -> axum::response::Response`, and
`wants_html(&http::HeaderMap) -> bool`.

**Out-of-band / invariants:** none out-of-band. No SQL and no new runtime dependency (sha2 and
base64 are dev-only).

**Model:** sonnet. The code is fully specified below.

- [x] **Step 1: Write the template and the module with its tests.** Create the two files below
  verbatim. The module holds `wants_html`/`accepts_html` (the Accept rule: exactly `text/html`,
  case-insensitive, q>0, a missing q means 1, an unparseable q means 0, any `Authorization`
  means false), `InstallPage::new` (replaces every placeholder with HTML-escaped values, origin
  from `url::Url::origin().ascii_serialization()`), `response()` (200 with the six headers), the
  `CSP` const with the two `'sha256-…'` sources, `VARY`, and eight unit tests.

  `crates/flags-server/src/install_page.html`:

````html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<title>Otto Flags connector</title>
<style>
:root {
  color-scheme: dark;
  --canvas: oklch(0.19 0.02 260);
  --surface: oklch(0.24 0.02 260);
  --raised: oklch(0.29 0.02 260);
  --edge: oklch(0.36 0.02 260);
  --ink: oklch(0.97 0.005 260);
  --muted: oklch(0.72 0.02 260);
  --faint: oklch(0.58 0.02 260);
  --accent: oklch(0.75 0.14 220);
  --accent-ink: oklch(0.2 0.05 240);
  --sans: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  --mono: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
}
* { box-sizing: border-box; }
html { background: var(--canvas); }
body {
  margin: 0;
  background: var(--canvas);
  color: var(--ink);
  font: 16px/1.6 var(--sans);
}
main {
  max-width: 600px;
  margin: 0 auto;
  padding: 48px max(16px, env(safe-area-inset-left)) 64px;
}
.eyebrow {
  margin: 0 0 8px;
  color: var(--muted);
  font-size: 14px;
  letter-spacing: 0.04em;
}
h1 { margin: 0 0 16px; font-size: 30px; line-height: 1.2; }
h2 { margin: 32px 0 8px; font-size: 18px; }
p, ol { margin: 0 0 16px; }
.lead { color: var(--muted); font-size: 18px; }
ol { padding-left: 24px; }
li { margin: 4px 0; }
a { color: var(--accent); text-underline-offset: 3px; }
code, .value {
  font-family: var(--mono);
  font-size: 14px;
  overflow-wrap: anywhere;
}
.copy {
  margin: 0 0 8px;
  padding: 12px;
  background: var(--surface);
  border: 1px solid var(--edge);
  border-radius: 8px;
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
  align-items: center;
}
.value {
  flex: 1 1 240px;
  min-width: 0;
  color: var(--ink);
  user-select: all;
}
button {
  flex: none;
  padding: 8px 16px;
  border: 0;
  border-radius: 6px;
  background: var(--accent);
  color: var(--accent-ink);
  font: 600 14px/1.2 var(--sans);
  cursor: pointer;
}
button:hover { filter: brightness(1.08); }
a:focus-visible, button:focus-visible, .value:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
  border-radius: 4px;
}
.status { min-height: 1.6em; margin: 0 0 16px; color: var(--muted); font-size: 14px; }
.links { display: flex; flex-wrap: wrap; gap: 8px 24px; margin-top: 32px; }
footer {
  margin-top: 40px;
  padding-top: 16px;
  border-top: 1px solid var(--edge);
  color: var(--muted);
  font-size: 14px;
}
footer code { color: var(--ink); background: var(--raised); padding: 1px 4px; border-radius: 4px; }
</style>
</head>
<body>
<main>
<p class="eyebrow">Otto Flags · connector</p>
<h1>Use Otto Flags from your AI assistant</h1>
<p class="lead">Otto Flags doesn't have a web page here. Add it to your AI assistant as a connector, then ask which flags are on, roll one out, or roll one back in plain language. The first time, you sign in with your Otto account.</p>

<h2 id="address-heading">Connector address</h2>
<div class="copy">
<code class="value" id="connector-url" tabindex="0" aria-labelledby="address-heading">{{CONNECTOR_URL}}</code>
<button type="button" data-copy="connector-url" data-status="connector-url-status">Copy</button>
</div>
<p class="status" id="connector-url-status" role="status"></p>

<h2>Claude (claude.ai or the desktop app)</h2>
<ol>
<li>Open Settings, then Connectors.</li>
<li>Choose Add custom connector.</li>
<li>Paste the connector address.</li>
<li>Choose Connect.</li>
<li>Sign in with your Otto account.</li>
</ol>

<h2>ChatGPT</h2>
<ol>
<li>Open Settings, then Connectors (turn on developer mode if Connectors asks for it).</li>
<li>Choose Create.</li>
<li>Paste the connector address.</li>
<li>Sign in with your Otto account.</li>
</ol>

<h2 id="claude-code-heading">Claude Code</h2>
<ol>
<li>Run this in your terminal:
<div class="copy">
<code class="value" id="claude-code-command" tabindex="0" aria-labelledby="claude-code-heading">claude mcp add --transport http otto-flags {{CONNECTOR_URL}}</code>
<button type="button" data-copy="claude-code-command" data-status="claude-code-command-status">Copy</button>
</div>
<p class="status" id="claude-code-command-status" role="status"></p>
</li>
<li>In Claude Code, run <code>/mcp</code> and sign in with your Otto account.</li>
</ol>

<p class="links">
<a href="{{PLATFORM_ORIGIN}}/products/flags#connect">Full setup guide</a>
<a href="{{PLATFORM_ORIGIN}}/">Open your Otto console</a>
</p>

<footer>
<p>Developers: the MCP endpoint is <code>{{CONNECTOR_URL}}</code>. Apps evaluate flags through the SDKs: see <code>/api/sdk/flags</code>. OAuth metadata is at <code>/.well-known/oauth-protected-resource</code>.</p>
</footer>
</main>
<script>
document.querySelectorAll("button[data-copy]").forEach(function (button) {
  button.addEventListener("click", function () {
    var target = document.getElementById(button.dataset.copy);
    var status = document.getElementById(button.dataset.status);
    var text = target.textContent;
    function selectIt() {
      var range = document.createRange();
      range.selectNodeContents(target);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      status.textContent = "Selected. Press Ctrl+C to copy.";
    }
    if (!navigator.clipboard || !navigator.clipboard.writeText) {
      selectIt();
      return;
    }
    navigator.clipboard.writeText(text).then(function () {
      status.textContent = "Copied.";
    }, selectIt);
  });
});
</script>
</body>
</html>
````

  `crates/flags-server/src/install_page.rs`:

````rust
//! The page a person's browser gets at `/`: how to add otto-flags to an AI
//! assistant as a connector (#71).
//!
//! otto-flags has no web console. People reach it from an AI assistant, and
//! apps reach it through the SDKs, but "Open console" links on the platform
//! send browsers to this origin, where the JSON 404 tells a person nothing.
//! So `/` serves this page, and only to a request that looks like a person's
//! browser: an `Accept` naming `text/html` with q>0 and no `Authorization`.
//! Agents, SDKs, and curl (`*/*`) keep the JSON 404 they always got.
//!
//! The page is one static file with an inline style and one inline script,
//! both allowed by hash in [`CSP`], so nothing else can run or load. The
//! connector address and the platform origin are substituted once, at
//! startup, outside both elements, so the hashes never depend on
//! configuration. A unit test recomputes them from the template.

use axum::response::{IntoResponse, Response};
use http::header::{self, HeaderMap, HeaderValue};
use http::StatusCode;

const TEMPLATE: &str = include_str!("install_page.html");

/// `'sha256-…'` of the exact text inside the template's `<style>` and
/// `<script>`. Editing either element fails
/// `the_csp_hashes_match_the_inline_style_and_script` until these are
/// updated to the values it prints.
pub const CSP: &str = "default-src 'none'; \
     style-src 'sha256-biNkFyEF7Iq98bZZN5Vj88IC/ok7B7JXp5K8/jV8nDk='; \
     script-src 'sha256-6/v0lPQ+Nr4A5QSG6MbNrYfKbg/E+eV/eVa95WpMYzQ='; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// What `/` varies on, for this page and for the JSON 404 it otherwise
/// answers: without it a cache could hand one to a client that asked for
/// the other.
pub const VARY: &str = "Accept, Authorization";

/// The rendered page, built once at startup.
#[derive(Debug, Clone)]
pub struct InstallPage {
    html: String,
}

impl InstallPage {
    /// `connector_url` is the address an AI assistant connects to (the
    /// service's resource URI); `platform_url` is the otto platform, whose
    /// origin the page links to. Both are HTML-escaped.
    pub fn new(connector_url: &str, platform_url: &str) -> Self {
        let html = TEMPLATE
            .replace("{{CONNECTOR_URL}}", &escape(connector_url))
            .replace("{{PLATFORM_ORIGIN}}", &escape(&origin(platform_url)));
        Self { html }
    }

    pub fn html(&self) -> &str {
        &self.html
    }

    /// 200 with the page and its security and caching headers. axum drops
    /// the body for `HEAD`.
    pub fn response(&self) -> Response {
        (
            StatusCode::OK,
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"),
                ),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=3600"),
                ),
                (header::VARY, HeaderValue::from_static(VARY)),
                (
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                ),
                (
                    header::REFERRER_POLICY,
                    HeaderValue::from_static("no-referrer"),
                ),
                (
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static(CSP),
                ),
            ],
            self.html.clone(),
        )
            .into_response()
    }
}

/// Whether the request is a person's browser landing on the origin: some
/// `Accept` header lists `text/html` with q>0, and there is no
/// `Authorization` header at all.
///
/// `text/*` and `*/*` do not count: curl and most HTTP libraries send `*/*`
/// and must keep getting JSON. A `q` that does not parse counts as 0, so a
/// malformed header gets JSON too.
pub fn wants_html(headers: &HeaderMap) -> bool {
    if headers.contains_key(header::AUTHORIZATION) {
        return false;
    }
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(accepts_html)
}

/// One media range, e.g. `text/html;q=0.9`.
fn accepts_html(range: &str) -> bool {
    let mut parts = range.split(';');
    let media = parts.next().unwrap_or("").trim();
    if !media.eq_ignore_ascii_case("text/html") {
        return false;
    }
    let mut q = 1.0_f32;
    for param in parts {
        let Some((name, value)) = param.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("q") {
            q = value.trim().parse().unwrap_or(0.0);
        }
    }
    q > 0.0
}

/// `https://otto.savvagent.com/anything` → `https://otto.savvagent.com`.
fn origin(platform_url: &str) -> String {
    match url::Url::parse(platform_url) {
        Ok(u) if u.origin().is_tuple() => u.origin().ascii_serialization(),
        _ => platform_url.trim_end_matches('/').to_string(),
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn accept(v: &str) -> bool {
        wants_html(&headers(&[("accept", v)]))
    }

    #[test]
    fn browsers_want_html() {
        // Chrome, Firefox, Safari navigations.
        assert!(accept(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,\
             image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"
        ));
        assert!(accept("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"));
        assert!(accept("TEXT/HTML"));
        assert!(accept("application/json, text/html ; q=0.1"));
    }

    #[test]
    fn everything_else_does_not() {
        assert!(!wants_html(&HeaderMap::new()));
        assert!(!accept("*/*"));
        assert!(!accept("application/json"));
        assert!(!accept("text/*"));
        assert!(!accept("text/html;q=0"));
        assert!(!accept("text/html;q=0.000"));
        assert!(!accept("text/html;q=junk"));
        assert!(!accept("text/htmlx"));
        assert!(!accept(""));
    }

    #[test]
    fn every_accept_header_is_read() {
        assert!(wants_html(&headers(&[
            ("accept", "application/json"),
            ("accept", "text/html"),
        ])));
    }

    #[test]
    fn any_authorization_means_not_a_browser_landing() {
        assert!(!wants_html(&headers(&[
            ("accept", "text/html"),
            ("authorization", "Bearer x"),
        ])));
        assert!(!wants_html(&headers(&[
            ("accept", "text/html"),
            ("authorization", ""),
        ])));
    }

    #[test]
    fn the_page_names_the_connector_and_links_the_platform() {
        let page = InstallPage::new(
            "https://flags.example.com/mcp",
            "https://platform.example.com/some/path",
        );
        let html = page.html();
        assert!(!html.contains("{{"), "a placeholder is left");
        assert_eq!(html.matches("https://flags.example.com/mcp").count(), 3);
        assert!(html.contains(
            "claude mcp add --transport http otto-flags https://flags.example.com/mcp"
        ));
        assert!(html.contains("href=\"https://platform.example.com/products/flags#connect\""));
        assert!(html.contains("href=\"https://platform.example.com/\""));
    }

    #[test]
    fn substituted_values_are_escaped() {
        let page = InstallPage::new(
            "https://flags.example.com/mcp?a=1&b=\"<x>'",
            "https://platform.example.com",
        );
        assert!(page
            .html()
            .contains("https://flags.example.com/mcp?a=1&amp;b=&quot;&lt;x&gt;&#39;"));
        assert!(!page.html().contains("<x>"));
    }

    fn inner<'a>(html: &'a str, tag: &str) -> &'a str {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        assert_eq!(html.matches(&open).count(), 1, "exactly one <{tag}>");
        let start = html.find(&open).unwrap() + open.len();
        let end = html[start..].find(&close).unwrap() + start;
        &html[start..end]
    }

    fn hash(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(s.as_bytes()))
    }

    #[test]
    fn the_csp_hashes_match_the_inline_style_and_script() {
        for tag in ["style", "script"] {
            let body = inner(TEMPLATE, tag);
            assert!(!body.contains("{{"), "<{tag}> must not hold a placeholder");
            let source = format!("'sha256-{}'", hash(body));
            assert!(
                CSP.contains(&source),
                "update CSP: the template's <{tag}> now hashes to {source}"
            );
        }
        assert_eq!(TEMPLATE.matches("<script").count(), 1);
        assert_eq!(TEMPLATE.matches("<style").count(), 1);
        assert_eq!(TEMPLATE.matches("<h1").count(), 1);
        // The hashes cover only the two elements: a style attribute or an
        // inline handler would be blocked, silently breaking the page.
        assert!(!TEMPLATE.contains(" style="), "no style attributes");
        assert!(
            !TEMPLATE
                .split(|c: char| c.is_whitespace())
                .any(|w| w.starts_with("on") && w.contains('=')),
            "no inline event handlers"
        );
    }

    #[test]
    fn the_response_carries_its_headers() {
        let res = InstallPage::new("https://f.example/mcp", "https://p.example").response();
        assert_eq!(res.status(), StatusCode::OK);
        let h = res.headers();
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(h[header::CACHE_CONTROL], "public, max-age=3600");
        assert_eq!(h[header::VARY], VARY);
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(h[header::CONTENT_SECURITY_POLICY], CSP);
    }
}
````

  To see the hash test fail first, temporarily change one character inside the template's
  `<script>` (for example `"Copied."` to `"Copied!"`). Add the module and dependency lines listed
  under Files.

- [x] **Step 2: Run the tests and see the hash test fail.**
  Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --lib install_page`
  Expected: `the_csp_hashes_match_the_inline_style_and_script` FAILS with "update CSP: the
  template's <script> now hashes to …". The other tests pass.

- [x] **Step 3: Revert the temporary change.** Change the script text back to `"Copied."` so the
  template matches the `CSP` constant.

- [x] **Step 4: Run the tests and see them pass.**
  Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --lib install_page`
  Expected: 8 passed, 0 failed, 0 ignored. Then run
  `cargo clippy -p flags-server --all-targets -- -D warnings` and `cargo fmt --all`, and fix any
  findings without suppressions.

- [x] **Step 5: Commit.**
  ```bash
  git add Cargo.toml Cargo.lock crates/flags-server/Cargo.toml crates/flags-server/src/install_page.rs crates/flags-server/src/install_page.html crates/flags-server/src/lib.rs
  git commit -m "feat(server): add the connector install page" -m "Browsers that open the otto-flags origin get a JSON 404 that tells a person nothing. This adds the static page and the Accept rule that decides who gets it; the next commit routes it. The CSP allows the one inline style and script by hash, and a test recomputes both from the template so an edit cannot silently break the page.

  Refs #71"
  ```

### Task 2: Route `/` to the page

**Files:**
- Modify: `crates/flags-server/src/lib.rs`: the module doc table (lines 3-10), `router()`
  (lines 51-70), a new `root` handler next to `not_found`, and new tests in the `tests` module
- Test: the `tests` module in `crates/flags-server/src/lib.rs`

**Interfaces:** consumes `install_page::{InstallPage, wants_html, VARY}` from Task 1.

**Out-of-band / invariants:** MCP auth is unchanged, and a test re-asserts the `/mcp` 401. No
route other than `/` changes. `POST /` must stay a 404, not a 405, with no `Allow` header.

**Model:** sonnet.

- [x] **Step 1: Write the failing router tests.** In `lib.rs` `mod tests`, add a helper and these
  tests:

```rust
    const BROWSER: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

    async fn root(
        method: http::Method,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, http::HeaderMap, Vec<u8>) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "flags.example.com");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = app()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
    }

    fn the_json_404(path: &str) -> serde_json::Value {
        serde_json::json!({
            "error": "not_found",
            "error_description": format!(
                "no route serves {path}. Agents: the MCP endpoint is /mcp (see \
                 /.well-known/oauth-protected-resource). SDKs: see /api/sdk/flags."
            ),
        })
    }

    fn json(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice(body).expect("a JSON body")
    }

    #[tokio::test]
    async fn a_browser_at_the_root_gets_the_install_page() {
        let (status, h, body) = root(http::Method::GET, "/", &[("accept", BROWSER)]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(h["content-type"], "text/html; charset=utf-8");
        assert_eq!(h["cache-control"], "public, max-age=3600");
        assert_eq!(h["vary"], "Accept, Authorization");
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(h["referrer-policy"], "no-referrer");
        assert_eq!(h["content-security-policy"], install_page::CSP);
        let html = String::from_utf8(body).unwrap();
        assert!(html.contains("<h1>Use Otto Flags from your AI assistant</h1>"));
        assert!(html.contains(">https://flags.example.com/mcp</code>"));
        assert!(html.contains("href=\"https://platform.example.com/products/flags#connect\""));
    }

    #[tokio::test]
    async fn head_at_the_root_gets_the_page_headers_and_no_body() {
        let (status, h, body) = root(http::Method::HEAD, "/", &[("accept", BROWSER)]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(h["content-type"], "text/html; charset=utf-8");
        assert_eq!(h["content-security-policy"], install_page::CSP);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn everything_else_at_the_root_keeps_the_json_404() {
        for headers in [
            &[][..],
            &[("accept", "*/*")][..],
            &[("accept", "application/json")][..],
            &[("accept", BROWSER), ("authorization", "Bearer otto_at_x")][..],
        ] {
            let (status, h, body) = root(http::Method::GET, "/", headers).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{headers:?}");
            assert_eq!(json(&body), the_json_404("/"), "{headers:?}");
            assert_eq!(h["vary"], "Accept, Authorization", "{headers:?}");
        }
        let (status, h, body) = root(http::Method::POST, "/", &[("accept", BROWSER)]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json(&body), the_json_404("/"));
        assert!(h.get("allow").is_none());
    }

    #[tokio::test]
    async fn other_paths_keep_the_json_404_for_browsers_too() {
        let (status, h, body) = root(http::Method::GET, "/nope", &[("accept", BROWSER)]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json(&body), the_json_404("/nope"));
        assert!(h.get("vary").is_none());
    }
```

  Also extend `an_unauthenticated_mcp_call_is_told_where_to_authenticate`: add
  `.header("accept", "text/html")` to its request. It must still get 401 with the
  `resource_metadata` challenge.

- [x] **Step 2: Run the tests and see them fail.**
  Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --lib`
  Expected: `a_browser_at_the_root_gets_the_install_page` and
  `head_at_the_root_gets_the_page_headers_and_no_body` FAIL (404).
  `everything_else_at_the_root_keeps_the_json_404` FAILS on the missing `vary`. The `/nope` and
  `/mcp` tests pass.

- [x] **Step 3: Implement the route.** In `lib.rs`:
  - imports: add `use axum::routing::any;`, and replace the existing `use http::{StatusCode, Uri};`
    with `use http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};`
  - in `router()`, before `health::router(db)`:
    ```rust
    let page = Arc::new(install_page::InstallPage::new(
        &config.resource_uri,
        &config.platform_url,
    ));
    ```
    and chain, right after `.merge(api)`:
    ```rust
        // `any`, not `get`: a GET-only route would answer other methods
        // with 405 (and add `Allow` to every response); `root` keeps them
        // on the JSON 404.
        .route(
            "/",
            any(move |method: Method, headers: HeaderMap, uri: Uri| {
                root(page.clone(), method, headers, uri)
            }),
        )
    ```
  - add the handler below `not_found`:
    ```rust
    /// `/`: the connector install page for a person's browser, the JSON 404
    /// for everyone else (see [`install_page::wants_html`]). Both vary on
    /// `Accept` and `Authorization`, so a cache never swaps one for the other.
    async fn root(
        page: Arc<install_page::InstallPage>,
        method: Method,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        if method != Method::GET && method != Method::HEAD {
            return not_found(uri).await;
        }
        if install_page::wants_html(&headers) {
            return page.response();
        }
        let mut res = not_found(uri).await;
        res.headers_mut()
            .insert(header::VARY, HeaderValue::from_static(install_page::VARY));
        res
    }
    ```
  - In the module doc table, add the line
    `//!   /                                 install page  browsers (Accept: text/html, no Authorization); else JSON 404`
    before the `everything else` line.

- [x] **Step 4: Run the tests and see them pass.**
  Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --lib`
  Expected: every lib test passes, including the four new ones and the extended `/mcp` test, with
  0 ignored. Run `cargo fmt --all`, then run the full gates from Global Constraints (test, clippy, fmt check, the
  migrations diff, and `podman build -t otto-flags:check .`) and record the counts per test
  binary.

- [x] **Step 5: Commit.**
  ```bash
  git add crates/flags-server/src/lib.rs
  git commit -m "feat(server): serve the install page at / to browsers" -m "\"Open console\" on the platform sends people to this origin. GET and HEAD / now serve the install page when Accept names text/html and there is no Authorization; every other request to / keeps the JSON 404 (now with Vary, so a cache cannot swap the two), and POST / stays a 404 rather than becoming a 405.

  Fixes #71"
  ```
