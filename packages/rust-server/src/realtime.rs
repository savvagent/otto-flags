//! The flag-change stream, `GET /api/flags/stream` (`docs/SDK-DEVELOPER-GUIDE.md`, Real-time Updates),
//! which invalidates cached evaluations so a flag change, a kill switch above all, reaches the
//! client in about a round trip instead of after the cache TTL (#167).
//!
//! One background task per client family (every clone shares it) holds one long-lived
//! connection and ends when the last clone is dropped, as the telemetry flusher does. A
//! `flag.created`, `flag.updated` or `flag.deleted` event invalidates that flag through the
//! cache's generation counters, so an evaluation in flight when it arrived is not cached either
//! (#137). Each `connected` event (the server sends one on every connect) invalidates the whole
//! cache: events may have been missed while the stream was down, and the TTL bounds staleness in
//! the meantime. Lookups never wait on the stream: the task only writes to the cache.
//!
//! A dropped or refused stream is retried with exponential backoff and jitter; the attempt count
//! resets only after a connection stayed up [`STABLE_AFTER`], so a server that accepts and drops
//! at once is not retried at the base delay forever. Stream failures are logged and swallowed;
//! they never open the evaluate cooldown (`crate::backoff`), which is about lookups. The key is
//! sent only in the `Authorization` header and never logged.

use crate::backoff::parse_retry_after;
use crate::cache::Cache;
use crate::client::is_valid_flag_key;
use reqwest::header::{ACCEPT, AUTHORIZATION, RETRY_AFTER};
use reqwest::StatusCode;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// The server sends a heartbeat every 30 s; three missed ones mean the connection is dead
/// even if the socket has not noticed.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// One event carries one flag with its variations and their configurations: generous, but
/// finite, so a broken or hostile peer cannot grow the buffer without bound.
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// The first retry waits half of this to all of it.
const BASE_DELAY: Duration = Duration::from_secs(1);
/// The longest wait between attempts after a dropped stream or a transport error.
const MAX_DELAY: Duration = Duration::from_secs(30);
/// The longest wait after a 401, 403 or 404: the key is refused or the endpoint absent, and
/// retrying fast changes nothing.
const REJECTED_MAX_DELAY: Duration = Duration::from_secs(300);
/// The longest a `Retry-After` can stretch one wait, so a proxy cannot hold the stream off for
/// hours.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// A connection that stayed up this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(30);
/// How much of a skipped event's data a debug log line shows.
const LOGGED_DATA_CHARS: usize = 80;

/// One dispatched server-sent event.
#[derive(Debug)]
pub(crate) struct SseEvent {
    /// The `event` field, `message` when the event had none.
    pub event: String,
    /// The `data` lines, joined with `\n`.
    pub data: String,
}

/// A pending event grew past [`MAX_EVENT_BYTES`]; the connection is dropped.
#[derive(Debug)]
pub(crate) struct TooLarge;

/// Incremental `text/event-stream` framing. A chunk may end anywhere, even inside a UTF-8
/// character, so bytes are buffered and a line is decoded only once its `\n` has arrived.
#[derive(Default)]
pub(crate) struct SseParser {
    /// The bytes of the line not yet terminated.
    line: Vec<u8>,
    /// The pending event's name.
    event: Option<String>,
    /// The pending event's data lines.
    data: Vec<String>,
    /// The bytes held in `data`, counting one joining `\n` per line.
    data_bytes: usize,
}

impl SseParser {
    /// Feed the next chunk of the body and return the events it completed.
    pub(crate) fn feed(&mut self, mut bytes: &[u8]) -> Result<Vec<SseEvent>, TooLarge> {
        let mut events = Vec::new();
        while let Some(end) = bytes.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&bytes[..end]);
            self.check_size()?;
            bytes = &bytes[end + 1..];
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(event) = self.process(&String::from_utf8_lossy(&line)) {
                events.push(event);
            }
            // Keep the allocation for the next line.
            line.clear();
            self.line = line;
        }
        self.line.extend_from_slice(bytes);
        self.check_size()?;
        Ok(events)
    }

    fn check_size(&self) -> Result<(), TooLarge> {
        if self.line.len() + self.data_bytes > MAX_EVENT_BYTES {
            Err(TooLarge)
        } else {
            Ok(())
        }
    }

    /// Apply one complete line; an empty line dispatches the pending event.
    fn process(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            let event = self.event.take();
            let data = std::mem::take(&mut self.data);
            self.data_bytes = 0;
            if event.is_none() && data.is_empty() {
                return None;
            }
            return Some(SseEvent {
                event: event.unwrap_or_else(|| "message".to_string()),
                data: data.join("\n"),
            });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                self.data_bytes += value.len() + 1;
                self.data.push(value.to_string());
            }
            // `id` and `retry` are not used: a reconnect clears the cache rather than resuming,
            // and the delay is the client's own.
            _ => {}
        }
        None
    }
}

/// The wait before reconnect attempt `attempt` (from 0): `min(BASE_DELAY * 2^attempt, cap)`,
/// scaled into `[50%, 100%]` by `unit` in `[0, 1)`, so clients dropped together (a deploy drops
/// every stream) do not reconnect together. A `unit` out of range is clamped, never a panic.
pub(crate) fn reconnect_delay(attempt: u32, cap: Duration, unit: f64) -> Duration {
    let full = BASE_DELAY
        .saturating_mul(2u32.saturating_pow(attempt))
        .min(cap);
    let unit = if unit.is_nan() {
        0.0
    } else {
        unit.clamp(0.0, 1.0)
    };
    full.mul_f64(0.5 + unit / 2.0)
}

/// A value in `[0, 1)` for jitter, from the standard library's randomly keyed hasher (each
/// `RandomState` gets new keys), so the crate needs no random-number dependency.
fn unit_random() -> f64 {
    let bits = RandomState::new().build_hasher().finish();
    (bits as f64 / u64::MAX as f64).min(1.0 - f64::EPSILON)
}

/// `<base_url>/api/flags/stream`, keeping any path prefix in `base_url`; `None` when
/// `base_url` is not a URL that can carry a path.
pub(crate) fn stream_url(base_url: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["api", "flags", "stream"]);
    Some(url)
}

/// The key of a flag event, if its data is JSON with a string `key` that a flag can have.
/// Event text is untrusted: it is only ever used as a cache key, and only after this check.
fn changed_flag_key(data: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let key = value.get("key")?.as_str()?;
    is_valid_flag_key(key).then(|| key.to_string())
}

/// Apply one event to the cache. `heartbeat` and events this SDK does not know are ignored.
/// What an event does to the cache.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Invalidation {
    All,
    Flag(String),
}

/// `None` for an event that changes nothing: a heartbeat, an unknown event, or a `flag.*` event
/// without a usable flag key.
pub(crate) fn invalidation_for(event: &SseEvent) -> Option<Invalidation> {
    match event.event.as_str() {
        "connected" => Some(Invalidation::All),
        "flag.created" | "flag.updated" | "flag.deleted" => {
            let key = changed_flag_key(&event.data);
            if key.is_none() {
                tracing::debug!(
                    "Otto Flags stream: {} event without a valid flag key skipped: {:?}",
                    event.event,
                    event
                        .data
                        .chars()
                        .take(LOGGED_DATA_CHARS)
                        .collect::<String>()
                );
            }
            key.map(Invalidation::Flag)
        }
        _ => None,
    }
}

fn handle(cache: &Cache, event: &SseEvent) {
    match invalidation_for(event) {
        Some(Invalidation::All) => cache.invalidate(None),
        Some(Invalidation::Flag(key)) => cache.invalidate(Some(&key)),
        None => {}
    }
}

#[derive(Clone)]
pub(crate) struct StreamTarget {
    /// The cache the events invalidate.
    pub cache: Cache,
    /// A client with a connect timeout and no total timeout, which would cut the stream off.
    pub http: reqwest::Client,
    /// From [`stream_url`].
    pub url: reqwest::Url,
    /// The app's server or client key.
    pub api_key: String,
}

struct Inner {
    /// Shared with the task, which must not keep `Inner` alive while it waits.
    shutdown: Arc<Notify>,
    /// Whether a task is running; reset when it ends, so a later call in another runtime
    /// starts a new one.
    running: Arc<AtomicBool>,
}

impl Drop for Inner {
    /// Wake the task so it closes the stream now. Heartbeats keep the idle timeout from ever
    /// firing, so without this an open stream would outlive the client.
    fn drop(&mut self) {
        self.shutdown.notify_one();
    }
}

/// Clears `running` however the task ends: normally, or dropped with its runtime.
struct ResetOnDrop(Arc<AtomicBool>);

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The stream of one client family, shared by its clones.
#[derive(Clone)]
pub(crate) struct Realtime {
    inner: Arc<Inner>,
}

impl Realtime {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                shutdown: Arc::new(Notify::new()),
                running: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// Start the stream task if none is running and a Tokio runtime is current; otherwise do
    /// nothing (a later call inside a runtime starts it). Called on every lookup, so the common
    /// case is one atomic load.
    pub(crate) fn ensure_started(&self, target: &StreamTarget) {
        if self.inner.running.load(Ordering::Acquire) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let client: Weak<Inner> = Arc::downgrade(&self.inner);
        let shutdown = Arc::clone(&self.inner.shutdown);
        // Built outside the task so it also runs if the task is dropped before its first poll.
        let reset = ResetOnDrop(Arc::clone(&self.inner.running));
        let target = target.clone();
        runtime.spawn(async move {
            let _reset = reset;
            run(client, shutdown, target).await;
        });
    }
}

/// How one connection attempt ended.
enum Ended {
    /// The request failed before a response (refused, unreachable, connect timeout).
    Unreachable,
    /// The server answered with a non-2xx status.
    Refused {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
    /// The stream was open from `since` until it ended (closed, broke, went idle, or sent an
    /// event over the cap).
    Dropped { since: Instant },
}

/// Connect, read, and reconnect until the client is gone. Every wait (the connect, each read,
/// the delay between attempts) is raced against `shutdown`, so the last client drop ends the
/// task at once.
async fn run(client: Weak<Inner>, shutdown: Arc<Notify>, target: StreamTarget) {
    let mut attempt: u32 = 0;
    loop {
        // Only a liveness check: holding the client across the waits would keep it alive.
        if client.upgrade().is_none() {
            return;
        }
        let ended = tokio::select! {
            ended = connection(&target) => ended,
            _ = shutdown.notified() => return,
        };
        let cap = match &ended {
            Ended::Dropped { since } => {
                if since.elapsed() >= STABLE_AFTER {
                    attempt = 0;
                }
                MAX_DELAY
            }
            Ended::Refused { status, .. }
                if matches!(
                    *status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
                ) =>
            {
                REJECTED_MAX_DELAY
            }
            _ => MAX_DELAY,
        };
        let mut delay = reconnect_delay(attempt, cap, unit_random());
        if let Ended::Refused {
            status: StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE,
            retry_after: Some(retry_after),
        } = ended
        {
            delay = delay.max(retry_after.min(MAX_RETRY_AFTER));
        }
        attempt = attempt.saturating_add(1);
        tracing::debug!("Otto Flags stream: reconnecting in {:?}", delay);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.notified() => return,
        }
    }
}

/// One connection: open the stream and apply its events until it ends. Every failure is
/// logged here and reported as how the attempt ended; nothing reaches the host.
async fn connection(target: &StreamTarget) -> Ended {
    let sent = target
        .http
        .get(target.url.clone())
        .header(AUTHORIZATION, format!("Bearer {}", target.api_key))
        .header(ACCEPT, "text/event-stream")
        .send()
        .await;
    let mut response = match sent {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!("Otto Flags stream: connect failed ({})", e.without_url());
            return Ended::Unreachable;
        }
    };
    let status = response.status();
    if !status.is_success() {
        if status == StatusCode::UNAUTHORIZED {
            tracing::error!(
                "Otto Flags stream: the API key was rejected (status {})",
                status
            );
        } else {
            tracing::warn!("Otto Flags stream: connect refused (status {})", status);
        }
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        return Ended::Refused {
            status,
            retry_after,
        };
    }
    tracing::info!("Otto Flags stream: connected");
    let since = Instant::now();
    let mut parser = SseParser::default();
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(bytes))) => match parser.feed(&bytes) {
                Ok(events) => events.iter().for_each(|e| handle(&target.cache, e)),
                Err(TooLarge) => {
                    tracing::warn!(
                        "Otto Flags stream: an event exceeded {} bytes; reconnecting",
                        MAX_EVENT_BYTES
                    );
                    break;
                }
            },
            Ok(Ok(None)) => {
                tracing::debug!("Otto Flags stream: closed by the server");
                break;
            }
            Ok(Err(e)) => {
                tracing::debug!("Otto Flags stream: read failed ({})", e.without_url());
                break;
            }
            Err(_) => {
                tracing::debug!(
                    "Otto Flags stream: nothing received for {:?}; reconnecting",
                    IDLE_TIMEOUT
                );
                break;
            }
        }
    }
    Ended::Dropped { since }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn parser_splits_events_across_arbitrary_chunk_boundaries() {
        let wire = "event: connected\ndata: {\"a\":1}\n\n: comment\nevent: flag.updated\r\ndata: {\"key\":\"f\"}\r\n\r\n";
        for split in 0..=wire.len() {
            let mut p = SseParser::default();
            let mut got = p.feed(&wire.as_bytes()[..split]).unwrap();
            got.extend(p.feed(&wire.as_bytes()[split..]).unwrap());
            let got: Vec<_> = got.into_iter().map(|e| (e.event, e.data)).collect();
            assert_eq!(
                got,
                vec![
                    ("connected".to_string(), "{\"a\":1}".to_string()),
                    ("flag.updated".to_string(), "{\"key\":\"f\"}".to_string()),
                ],
                "split at {split}"
            );
        }
    }

    #[test]
    fn parser_joins_data_lines_and_defaults_the_event_name() {
        let mut p = SseParser::default();
        let got = p.feed(b"data: a\ndata:b\n\n").unwrap();
        assert_eq!(
            (got[0].event.as_str(), got[0].data.as_str()),
            ("message", "a\nb")
        );
    }

    #[test]
    fn parser_drops_an_empty_event_and_ignores_other_fields() {
        let mut p = SseParser::default();
        let got = p
            .feed(b"\n\nid: 7\nretry: 10\n\nevent: heartbeat\nid: 8\ndata:  x\n\n")
            .unwrap();
        let got: Vec<_> = got.into_iter().map(|e| (e.event, e.data)).collect();
        // Only the leading space is stripped; `id` and `retry` alone make no event.
        assert_eq!(got, vec![("heartbeat".to_string(), " x".to_string())]);
    }

    #[test]
    fn parser_keeps_a_split_multibyte_character_intact() {
        let wire = "data: \u{e9}\n\n".as_bytes();
        let mut p = SseParser::default();
        let mut got = p.feed(&wire[..7]).unwrap();
        got.extend(p.feed(&wire[7..]).unwrap());
        assert_eq!(got[0].data, "\u{e9}");
    }

    #[test]
    fn parser_refuses_an_event_over_the_cap() {
        let mut p = SseParser::default();
        let big = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert!(p.feed(&big).is_err());
    }

    #[test]
    fn parser_counts_accumulated_data_lines_against_the_cap() {
        let mut p = SseParser::default();
        let line = format!("data: {}\n", "x".repeat(1024 * 1024));
        let mut refused = false;
        for _ in 0..5 {
            if p.feed(line.as_bytes()).is_err() {
                refused = true;
                break;
            }
        }
        assert!(
            refused,
            "five 1 MiB data lines with no dispatch were accepted"
        );
    }

    #[test]
    fn reconnect_delay_doubles_to_the_cap_with_jitter_in_the_upper_half() {
        let s = Duration::from_secs;
        assert!(reconnect_delay(0, MAX_DELAY, 0.0) == Duration::from_millis(500));
        assert!(reconnect_delay(0, MAX_DELAY, 0.999999) < s(1));
        assert!(reconnect_delay(3, MAX_DELAY, 0.0) == s(4));
        assert!(reconnect_delay(30, MAX_DELAY, 0.0) == s(15));
        assert!(reconnect_delay(30, REJECTED_MAX_DELAY, 0.0) == s(150));
        assert!(reconnect_delay(u32::MAX, MAX_DELAY, 0.5) <= MAX_DELAY);
    }

    #[test]
    fn reconnect_delay_survives_a_unit_out_of_range() {
        for unit in [f64::NAN, f64::INFINITY, -1.0, 2.0] {
            let d = reconnect_delay(2, MAX_DELAY, unit);
            assert!(
                d >= Duration::from_secs(2) && d <= Duration::from_secs(4),
                "{unit}: {d:?}"
            );
        }
    }

    #[test]
    fn unit_random_is_in_the_unit_interval() {
        for _ in 0..1_000 {
            let u = unit_random();
            assert!((0.0..1.0).contains(&u), "{u}");
        }
    }

    #[test]
    fn stream_url_keeps_a_path_prefix_and_rejects_garbage() {
        assert_eq!(
            stream_url("https://h.example/p/").unwrap().as_str(),
            "https://h.example/p/api/flags/stream"
        );
        assert_eq!(
            stream_url("https://h.example").unwrap().as_str(),
            "https://h.example/api/flags/stream"
        );
        assert!(stream_url("not a url").is_none());
    }

    fn event(name: &str, data: &str) -> SseEvent {
        SseEvent {
            event: name.into(),
            data: data.into(),
        }
    }

    fn cached(cache: &Cache, flag: &str) -> bool {
        cache.get_entry(flag, "ctx").is_some()
    }

    fn cache_with(flags: &[&str]) -> Cache {
        let cache = Cache::new(Duration::from_secs(60));
        for flag in flags {
            cache.set(flag, "ctx".into(), true, None, None, None);
        }
        cache
    }

    #[test]
    fn a_flag_event_invalidates_only_that_flag() {
        for name in ["flag.created", "flag.updated", "flag.deleted"] {
            let cache = cache_with(&["a", "b"]);
            handle(&cache, &event(name, r#"{"key":"a","version":3}"#));
            assert!(!cached(&cache, "a"), "{name}");
            assert!(cached(&cache, "b"), "{name}");
        }
    }

    #[test]
    fn connected_invalidates_every_flag() {
        let cache = cache_with(&["a", "b"]);
        handle(&cache, &event("connected", "{}"));
        assert!(!cached(&cache, "a") && !cached(&cache, "b"));
    }

    #[test]
    fn heartbeats_unknown_events_and_bad_keys_change_nothing() {
        let cache = cache_with(&["a"]);
        let before = cache.generation("a");
        for e in [
            event("heartbeat", "{}"),
            event("message", r#"{"key":"a"}"#),
            event("flag.updated", "not json"),
            event("flag.updated", r#"{"key":7}"#),
            event("flag.updated", r#"{"flag":"a"}"#),
            event("flag.updated", r#"{"key":"../a"}"#),
            event("flag.updated", r#"{"key":""}"#),
        ] {
            handle(&cache, &e);
        }
        assert!(cached(&cache, "a"));
        assert_eq!(cache.generation("a"), before);
    }

    #[test]
    fn invalidation_for_maps_events_to_cache_actions() {
        let of = |name, data| invalidation_for(&event(name, data));
        assert_eq!(of("connected", "{}"), Some(Invalidation::All));
        assert_eq!(
            of("flag.created", r#"{"key":"f"}"#),
            Some(Invalidation::Flag("f".to_string()))
        );
        assert_eq!(of("flag.updated", r#"{"key":"a/b"}"#), None);
        assert_eq!(of("flag.updated", "not json"), None);
        assert_eq!(of("heartbeat", "{}"), None);
    }

    fn target(url: &str) -> StreamTarget {
        StreamTarget {
            cache: Cache::new(Duration::from_secs(60)),
            http: reqwest::Client::new(),
            url: stream_url(url).unwrap(),
            api_key: "srv_test".into(),
        }
    }

    #[test]
    fn without_a_runtime_nothing_starts() {
        let realtime = Realtime::new();
        realtime.ensure_started(&target("http://127.0.0.1:9"));
        assert!(!realtime.inner.running.load(Ordering::SeqCst));
    }

    async fn wait_for_requests(server: &wiremock::MockServer, n: usize) -> bool {
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.received_requests().await.unwrap_or_default().len() < n {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test]
    async fn the_task_ends_when_the_client_is_dropped_even_mid_connect() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/flags/stream"))
            .and(header("authorization", "Bearer srv_test"))
            .and(header("accept", "text/event-stream"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        let realtime = Realtime::new();
        let clone = realtime.clone();
        realtime.ensure_started(&target(&server.uri()));
        let running = Arc::clone(&realtime.inner.running);
        assert!(running.load(Ordering::SeqCst));
        assert!(
            wait_for_requests(&server, 1).await,
            "the stream was never requested"
        );

        drop(realtime);
        drop(clone);
        let ended = tokio::time::timeout(Duration::from_secs(2), async {
            while running.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(ended.is_ok(), "the stream task kept running");
    }

    #[tokio::test]
    async fn a_running_task_is_not_started_twice() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/flags/stream"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        let realtime = Realtime::new();
        let target = target(&server.uri());
        realtime.ensure_started(&target);
        realtime.clone().ensure_started(&target);
        assert!(wait_for_requests(&server, 1).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
