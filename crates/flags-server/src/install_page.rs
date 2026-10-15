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

use axum::body::Bytes;
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
     script-src 'sha256-X+vtavDuvM5Xh6gw3SGFEn/6P358qCI6/LPP/D2GTh8='; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// What `GET`/`HEAD /` vary on, for this page and for the JSON 404 they
/// otherwise answer: without it a cache could hand one to a client that asked for
/// the other.
pub const VARY: &str = "Accept, Authorization";

/// The rendered page, built once at startup. `Bytes`, so each response
/// shares it rather than copying it.
#[derive(Debug, Clone)]
pub struct InstallPage {
    html: Bytes,
}

impl InstallPage {
    /// `connector_url` is the address an AI assistant connects to (the
    /// service's resource URI, trusted operator config that is shown, not
    /// parsed); `platform_url` is the otto platform, whose origin the page
    /// links to. Both are HTML-escaped. The origin goes in first: it is a
    /// serialized URL origin and cannot contain a `{{` that the connector
    /// substitution would then expand.
    pub fn new(connector_url: &str, platform_url: &str) -> Self {
        let html = TEMPLATE
            .replace("{{PLATFORM_ORIGIN}}", &escape(&origin(platform_url)))
            .replace("{{CONNECTOR_URL}}", &escape(connector_url));
        Self {
            html: Bytes::from(html),
        }
    }

    /// The rendered page, for tests.
    #[cfg(test)]
    fn html(&self) -> &str {
        std::str::from_utf8(&self.html).expect("the page is built from &str")
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
/// and must keep getting JSON. A `q` that does not parse, or is outside 0
/// to 1, counts as 0, so a malformed header gets JSON too. An `Accept` value
/// that is not visible ASCII is skipped (the others are still read). The
/// parse is deliberately lenient where leniency is harmless: a parameter
/// without `=` is ignored, the last `q` wins, and any number `f32` accepts
/// in 0 to 1 is a qvalue.
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
            // RFC 9110 qvalues are 0 to 1; anything else (junk, `inf`,
            // `NaN`, `5`) counts as 0.
            q = value
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|q| (0.0..=1.0).contains(q))
                .unwrap_or(0.0);
        }
    }
    q > 0.0
}

/// `https://otto.savvagent.com/anything` → `https://otto.savvagent.com`.
///
/// Config already refuses a `FLAGS_PLATFORM_URL` that is not an absolute
/// http(s) URL, so the empty fallback is reached only by a direct caller
/// passing something else. It makes the links same-origin paths rather than
/// letting an arbitrary string (say `javascript:…`) into an `href`.
fn origin(platform_url: &str) -> String {
    match url::Url::parse(platform_url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u.origin().ascii_serialization(),
        _ => String::new(),
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
        assert!(accept(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        ));
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
        assert!(!accept("text/html;q=inf"));
        assert!(!accept("text/html;q=NaN"));
        assert!(!accept("text/html;q=5"));
        assert!(!accept("text/htmlx"));
        assert!(!accept(""));
    }

    #[test]
    fn lenient_parameters_are_harmless() {
        assert!(!accept("text/html;level=1;q=0"));
        assert!(accept("text/html;level=1;q=0.5"));
        assert!(!accept("text/html;q=1;q=0"), "the last q wins");
        assert!(accept("text/html;q"), "a parameter without = is ignored");
    }

    #[test]
    fn a_non_ascii_accept_value_is_skipped_not_fatal() {
        let mut h = HeaderMap::new();
        h.append(
            header::ACCEPT,
            HeaderValue::from_bytes(b"text/html\xff").unwrap(),
        );
        assert!(!wants_html(&h));
        h.append(header::ACCEPT, HeaderValue::from_static("text/html"));
        assert!(wants_html(&h));
    }

    #[test]
    fn the_link_origin_is_the_platform_origin_or_nothing() {
        assert_eq!(origin("https://p.example/"), "https://p.example");
        assert_eq!(origin("https://p.example/a/b?c"), "https://p.example");
        assert_eq!(origin("http://localhost:8080/x"), "http://localhost:8080");
        assert_eq!(origin("javascript:alert(1)"), "");
        assert_eq!(origin("not a url"), "");
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
        assert!(html
            .contains("claude mcp add --transport http otto-flags https://flags.example.com/mcp"));
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
        let page = InstallPage::new("https://f.example/mcp", "javascript:alert('x')");
        assert!(page.html().contains("href=\"/products/flags#connect\""));
        assert!(!page.html().contains("javascript:"));
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
        // `default-src 'none'` blocks every external load, so the page has none.
        for external in ["<link", "<img", "src=", "url(", "javascript:"] {
            assert!(!TEMPLATE.contains(external), "no {external}");
        }
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
