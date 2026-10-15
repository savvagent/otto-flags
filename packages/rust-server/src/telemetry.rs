//! Evaluation and error telemetry, sent in batches to `/api/telemetry/evaluations` and
//! `/api/telemetry/errors` (`docs/SDK-DEVELOPER-GUIDE.md`, Telemetry).
//!
//! Auto-rollback compares the error rate with a flag on against the rate with it off, from these
//! reports; by default it counts only reports sent with the server key (`srv_`) (#93).
//!
//! Events queue in memory and leave in batches: every [`FLUSH_INTERVAL`] from a background task,
//! as soon as a queue holds [`FLUSH_AT`] events, and on [`Telemetry::flush`]. Telemetry never
//! blocks or fails a lookup: a full queue drops new events (counted and logged), and a failed
//! send is logged and dropped, never retried.

use crate::types::ErrorReport;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// How often the background task sends what is queued.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// A queue this long is sent without waiting for the interval.
const FLUSH_AT: usize = 100;
/// The server refuses batches above 1 000 events (`flags_core::telemetry::MAX_BATCH`).
const MAX_EVENTS_PER_REQUEST: usize = 1_000;
/// Well under the server's 2 MB JSON body limit, whatever the text in the events.
const MAX_REQUEST_BYTES: usize = 1 << 20;
/// Events kept per kind while waiting to be sent; more are dropped and counted.
const MAX_QUEUED: usize = 10_000;

// The server truncates to these lengths (characters) anyway; truncating first keeps requests
// small and the byte budget predictable.
const MAX_ERROR_TYPE: usize = 200;
const MAX_ERROR_MESSAGE: usize = 2_000;
const MAX_STACK: usize = 8_000;
const MAX_VARIATION: usize = 64;
const MAX_ENVIRONMENT: usize = 64;

#[derive(Clone, Copy)]
enum Kind {
    Evaluations,
    Errors,
}

impl Kind {
    /// The endpoint's last path segment, which is also the body's field name.
    fn name(self) -> &'static str {
        match self {
            Kind::Evaluations => "evaluations",
            Kind::Errors => "errors",
        }
    }
}

#[derive(Default)]
struct Queues {
    evaluations: Vec<Value>,
    errors: Vec<Value>,
    dropped: u64,
}

impl Queues {
    fn of(&mut self, kind: Kind) -> &mut Vec<Value> {
        match kind {
            Kind::Evaluations => &mut self.evaluations,
            Kind::Errors => &mut self.errors,
        }
    }
}

struct Inner {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    enabled: bool,
    queues: Mutex<Queues>,
    /// Shared with the background task, which must not keep `Inner` alive while it waits.
    wake: Arc<Notify>,
    /// Whether a background task is running; reset when it ends, so a later event in another
    /// runtime starts a new one.
    running: Arc<AtomicBool>,
}

impl Drop for Inner {
    /// Wake the background task so it sees the client is gone and ends now, not at its next
    /// tick.
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}

/// Clears `running` however the background task ends: normally, or dropped with its runtime.
struct ResetOnDrop(Arc<AtomicBool>);

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub(crate) struct Telemetry {
    inner: Arc<Inner>,
}

fn truncated(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn now_secs() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Insert `key` only when there is a value, so absent fields stay absent on the wire.
fn put(event: &mut Value, key: &str, value: Option<String>) {
    if let (Some(v), Some(map)) = (value, event.as_object_mut()) {
        map.insert(key.to_string(), Value::String(v));
    }
}

/// Split events into request bodies of at most [`MAX_EVENTS_PER_REQUEST`] events and about
/// [`MAX_REQUEST_BYTES`] bytes of JSON.
fn batches(events: Vec<Value>) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut bytes = 0;
    for event in events {
        let size = serde_json::to_vec(&event).map(|v| v.len()).unwrap_or(0) + 1;
        if !current.is_empty()
            && (current.len() == MAX_EVENTS_PER_REQUEST || bytes + size > MAX_REQUEST_BYTES)
        {
            out.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes += size;
        current.push(event);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

impl Telemetry {
    pub fn new(http: reqwest::Client, base_url: String, api_key: String, enabled: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                http,
                base_url,
                api_key,
                enabled,
                queues: Mutex::new(Queues::default()),
                wake: Arc::new(Notify::new()),
                running: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// Queue one answered lookup. No user id or context is sent: the server reads only these.
    pub fn track_evaluation(
        &self,
        flag_key: &str,
        result: bool,
        variation: Option<&str>,
        environment: Option<&str>,
    ) {
        if !self.inner.enabled {
            return;
        }
        let mut event = json!({ "flag_key": flag_key, "result": result });
        put(
            &mut event,
            "variation",
            variation.map(|v| truncated(v, MAX_VARIATION)),
        );
        put(
            &mut event,
            "environment",
            environment.map(|e| truncated(e, MAX_ENVIRONMENT)),
        );
        if let (Some(t), Some(map)) = (now_secs(), event.as_object_mut()) {
            map.insert("timestamp".into(), t.into());
        }
        self.push(Kind::Evaluations, event);
    }

    /// Queue an error the host raised behind a flag.
    pub fn track_error(&self, report: ErrorReport) {
        if !self.inner.enabled {
            return;
        }
        let mut event = json!({
            "flag_key": report.flag_key,
            "flag_enabled": report.flag_enabled,
            "error_type": truncated(&report.error_type, MAX_ERROR_TYPE),
            "error_message": truncated(&report.error_message, MAX_ERROR_MESSAGE),
        });
        put(
            &mut event,
            "stack_trace",
            report.stack_trace.map(|s| truncated(&s, MAX_STACK)),
        );
        put(
            &mut event,
            "environment",
            report.environment.map(|e| truncated(&e, MAX_ENVIRONMENT)),
        );
        if let (Some(t), Some(map)) = (now_secs(), event.as_object_mut()) {
            map.insert("timestamp".into(), t.into());
        }
        self.push(Kind::Errors, event);
    }

    fn push(&self, kind: Kind, event: Value) {
        let full = match self.inner.queues.lock() {
            Ok(mut queues) => {
                if queues.of(kind).len() >= MAX_QUEUED {
                    queues.dropped += 1;
                    return;
                }
                let queue = queues.of(kind);
                queue.push(event);
                queue.len() >= FLUSH_AT
            }
            Err(_) => return,
        };
        self.ensure_flusher();
        if full {
            self.inner.wake.notify_one();
        }
    }

    /// Start the background task if none is running and a Tokio runtime is current. Without a
    /// runtime the events wait for `flush()` or a later event queued inside one.
    fn ensure_flusher(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let client: Weak<Inner> = Arc::downgrade(&self.inner);
        let wake = Arc::clone(&self.inner.wake);
        // Built outside the task so it also runs if the task is dropped before its first poll.
        let reset = ResetOnDrop(Arc::clone(&self.inner.running));
        runtime.spawn(async move {
            let _reset = reset;
            loop {
                // `sleep`, not `interval`: an interval's first tick is immediate.
                tokio::select! {
                    _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
                    _ = wake.notified() => {}
                }
                // Hold the client only while sending, so dropping every clone ends the task.
                let Some(inner) = client.upgrade() else {
                    break;
                };
                Telemetry { inner }.flush().await;
            }
        });
    }

    /// Send everything queued now. Failures are logged and the events dropped.
    pub async fn flush(&self) {
        let (evaluations, errors, dropped) = match self.inner.queues.lock() {
            Ok(mut queues) => (
                std::mem::take(&mut queues.evaluations),
                std::mem::take(&mut queues.errors),
                std::mem::take(&mut queues.dropped),
            ),
            Err(_) => return,
        };
        if dropped > 0 {
            tracing::warn!(
                "Otto Flags telemetry queue was full; dropped {} events",
                dropped
            );
        }
        self.send(Kind::Evaluations, evaluations).await;
        self.send(Kind::Errors, errors).await;
    }

    async fn send(&self, kind: Kind, events: Vec<Value>) {
        if events.is_empty() {
            return;
        }
        let Some(url) = self.url(kind) else {
            tracing::warn!("Otto Flags telemetry not sent: invalid base_url");
            return;
        };
        for batch in batches(events) {
            let count = batch.len();
            let body = json!({ kind.name(): batch });
            let sent = self
                .inner
                .http
                .post(url.clone())
                .header("Authorization", format!("Bearer {}", self.inner.api_key))
                .json(&body)
                .send()
                .await;
            let failure = match sent {
                Ok(response) if response.status().is_success() => None,
                Ok(response) => Some(format!("status {}", response.status())),
                Err(e) => Some(e.without_url().to_string()),
            };
            if let Some(why) = failure {
                tracing::warn!(
                    "Otto Flags telemetry: {} {} not sent ({})",
                    count,
                    kind.name(),
                    why
                );
            }
        }
    }

    fn url(&self, kind: Kind) -> Option<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.inner.base_url).ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .extend(["api", "telemetry", kind.name()]);
        Some(url)
    }

    #[cfg(test)]
    fn queued(&self) -> (usize, usize, u64) {
        let queues = self.inner.queues.lock().unwrap();
        (
            queues.evaluations.len(),
            queues.errors.len(),
            queues.dropped,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry(enabled: bool) -> Telemetry {
        Telemetry::new(
            reqwest::Client::new(),
            "http://127.0.0.1:9".into(),
            "srv_test".into(),
            enabled,
        )
    }

    #[test]
    fn truncation_keeps_whole_characters() {
        let s = "\u{e9}".repeat(10);
        assert_eq!(truncated(&s, 4), "\u{e9}".repeat(4));
        assert_eq!(truncated("ab", 4), "ab");
    }

    #[test]
    fn a_full_queue_drops_and_counts_new_events() {
        // No runtime here, so nothing is sent while the queue fills.
        let t = telemetry(true);
        for _ in 0..MAX_QUEUED + 3 {
            t.track_evaluation("f", true, None, None);
        }
        assert_eq!(t.queued(), (MAX_QUEUED, 0, 3));
    }

    #[test]
    fn disabled_telemetry_queues_nothing() {
        let t = telemetry(false);
        t.track_evaluation("f", true, None, None);
        t.track_error(ErrorReport::new("f", true, "E", "m"));
        assert_eq!(t.queued(), (0, 0, 0));
    }

    #[test]
    fn batches_respect_the_event_and_byte_limits() {
        let small: Vec<Value> = (0..2_500).map(|i| json!({ "i": i })).collect();
        let sizes: Vec<usize> = batches(small).iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![1_000, 1_000, 500]);

        // Errors of maximum length in 4-byte characters: about 40 KB of JSON each.
        let big = |_| {
            json!({
                "error_type": "\u{1F600}".repeat(MAX_ERROR_TYPE),
                "error_message": "\u{1F600}".repeat(MAX_ERROR_MESSAGE),
                "stack_trace": "\u{1F600}".repeat(MAX_STACK),
            })
        };
        let out = batches((0..100).map(big).collect());
        assert!(out.len() > 1);
        assert_eq!(out.iter().map(Vec::len).sum::<usize>(), 100);
        for batch in &out {
            let bytes = serde_json::to_vec(&json!({ "errors": batch }))
                .unwrap()
                .len();
            assert!(bytes < 2 * 1024 * 1024, "{bytes} bytes");
        }
    }

    #[tokio::test]
    async fn the_background_task_ends_when_the_client_is_dropped() {
        let t = telemetry(true);
        t.track_evaluation("f", true, None, None);
        let running = Arc::clone(&t.inner.running);
        assert!(running.load(Ordering::SeqCst));

        drop(t);
        let ended = tokio::time::timeout(Duration::from_secs(2), async {
            while running.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(ended.is_ok(), "the background task kept running");
    }

    #[test]
    fn a_new_runtime_gets_a_new_background_task() {
        let t = telemetry(true);
        let first = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        first.block_on(async { t.track_evaluation("f", true, None, None) });
        assert!(t.inner.running.load(Ordering::SeqCst));
        drop(first);
        // Dropping the runtime dropped its task.
        assert!(!t.inner.running.load(Ordering::SeqCst));

        let second = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        second.block_on(async { t.track_evaluation("f", true, None, None) });
        assert!(t.inner.running.load(Ordering::SeqCst));
    }
}
