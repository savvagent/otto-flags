//! Old hosts that only redirect (`FLAGS_REDIRECT_HOSTS`, savvagent/otto-platform#135).
//!
//! After the rebrand the canonical origin is `FLAGS_PUBLIC_URL`
//! (`https://flags.savvagent.com`). The old `otto-flags.savvagent.com` stays
//! pointed at this service, but a request to it for anything except the SDK
//! API is answered `308 Permanent Redirect` to the same path and query on the
//! canonical origin: `/mcp`, the install page, `/.well-known/…`, `/llms.txt`,
//! the health routes, `/platform/webhooks`. 308 (not 301) keeps the method and
//! body, so a client that follows it POSTs again.
//!
//! The SDK API is the exception, because SDK builds that predate the rebrand
//! default to the old host, send POSTs with an SDK key, and most do not follow
//! redirects. Every route flags-api serves for SDKs is under [`SDK_PREFIX`]
//! (`/api/flags/{key}/evaluate`, `/api/evaluate/{key}`, `/api/sdk/flags`,
//! `/api/sdk/enterprise-flags`, `/api/flags/stream`, `/api/telemetry/…`), and
//! every SDK client in `packages/*` calls only paths under it. Those requests
//! pass through untouched. `/api/…` does not check `Host`, so they need no
//! entry in `FLAGS_ALLOWED_HOSTS`; the redirect hosts are deliberately absent
//! from it, so `/mcp` could not be served there even if it were not redirected
//! first.
//!
//! The `Location` is built from configuration and the request's raw path and
//! query only: never from the request's `Host`, scheme or forwarded headers.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{header, HeaderValue, StatusCode};

/// Every SDK-facing route lives under this prefix; requests for it are served
/// on a redirect host exactly as on the canonical one.
pub const SDK_PREFIX: &str = "/api/";

/// What the redirect layer needs, cloned into each request.
#[derive(Debug)]
pub struct Redirects {
    hosts: Vec<String>,
    /// `FLAGS_PUBLIC_URL`, without a trailing slash.
    origin: String,
}

impl Redirects {
    /// `None` when no host redirects, so the router takes no extra layer.
    pub fn new(hosts: &[String], public_url: &str) -> Option<Arc<Self>> {
        (!hosts.is_empty()).then(|| {
            Arc::new(Self {
                hosts: hosts.to_vec(),
                origin: public_url.trim_end_matches('/').to_string(),
            })
        })
    }

    fn redirects(&self, host: &str) -> bool {
        self.hosts.iter().any(|h| h == host)
    }
}

/// Whether `path` belongs to the SDK API.
pub fn is_sdk_path(path: &str) -> bool {
    path.starts_with(SDK_PREFIX)
}

/// The hostname of a request: the `Host` header, else the URI authority
/// (HTTP/2), lowercased, without a port or a trailing dot.
fn request_host(req: &Request) -> Option<String> {
    let raw = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))?;
    Some(bare_host(raw))
}

fn bare_host(raw: &str) -> String {
    let raw = raw.trim();
    let host = if let Some(rest) = raw.strip_prefix('[') {
        // `[::1]:8080`: keep the address, drop the port.
        rest.split(']').next().unwrap_or(rest)
    } else {
        raw.rsplit_once(':').map_or(raw, |(h, _)| h)
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

pub async fn layer(State(r): State<Arc<Redirects>>, req: Request, next: Next) -> Response {
    let on_redirect_host = request_host(&req).is_some_and(|h| r.redirects(&h));
    if !on_redirect_host || is_sdk_path(req.uri().path()) {
        return next.run(req).await;
    }
    let target = req.uri().path_and_query().map_or("/", |pq| pq.as_str());
    // `*` (an `OPTIONS *`) and other non-origin forms have no path to keep.
    let target = if target.starts_with('/') { target } else { "/" };
    match HeaderValue::try_from(format!("{}{target}", r.origin)) {
        Ok(location) => (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, location)],
        )
            .into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_compared_without_port_case_or_trailing_dot() {
        assert_eq!(
            bare_host("Otto-Flags.Savvagent.com:443"),
            "otto-flags.savvagent.com"
        );
        assert_eq!(
            bare_host("otto-flags.savvagent.com."),
            "otto-flags.savvagent.com"
        );
        assert_eq!(bare_host("[::1]:8080"), "::1");
        assert_eq!(bare_host("localhost"), "localhost");
    }

    #[test]
    fn only_paths_under_api_are_sdk_paths() {
        for p in [
            "/api/flags/x/evaluate",
            "/api/evaluate/x",
            "/api/sdk/flags",
            "/api/sdk/enterprise-flags",
            "/api/flags/stream",
            "/api/telemetry/evaluations",
            "/api/telemetry/errors",
        ] {
            assert!(is_sdk_path(p), "{p}");
        }
        for p in [
            "/",
            "/mcp",
            "/api",
            "/apis/x",
            "/platform/webhooks",
            "/llms.txt",
            "/.well-known/oauth-protected-resource",
        ] {
            assert!(!is_sdk_path(p), "{p}");
        }
    }
}
