package com.savvagent.ottoflags.sdk

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import okhttp3.Call
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okio.Buffer
import org.json.JSONObject
import java.io.IOException
import java.util.concurrent.TimeUnit
import kotlin.random.Random

/** One server-sent event: its `event` field ("message" when it had none) and its `data`. */
internal data class SseEvent(val event: String, val data: String)

/** An event (its pending line plus its data) grew past [SseParser.maxEventBytes]. */
internal class SseEventTooLargeException(limit: Int) :
    IOException("Otto Flags stream: an event exceeded $limit bytes")

/**
 * An incremental `text/event-stream` parser: feed it the body in chunks of any size, split
 * anywhere (a multi-byte character included), and it returns the events each chunk completed.
 *
 * Lines end in `\n` or `\r\n`; lines starting with `:` are comments (the server's heartbeats);
 * several `data` lines join with `\n`; an event with neither an `event` nor a `data` line is
 * dropped; `id` and `retry` are ignored (a reconnect clears the cache instead of resuming, and
 * the delay is the client's own). The pending line plus the pending data may not exceed
 * [maxEventBytes], so a peer cannot grow the buffers without bound. Not thread safe: one
 * parser per connection.
 */
internal class SseParser(private val maxEventBytes: Int = MAX_EVENT_BYTES) {
    /** The bytes of the line not yet terminated. */
    private val line = Buffer()
    private var event: String? = null
    private val data = StringBuilder()
    /** UTF-8 bytes in [data], which is what the cap counts. */
    private var dataBytes = 0L
    private var hasData = false

    /**
     * Feeds `bytes[offset until offset + length]` and returns the events it completed.
     *
     * @throws SseEventTooLargeException when the pending event passes the cap
     */
    fun feed(bytes: ByteArray, offset: Int = 0, length: Int = bytes.size - offset): List<SseEvent> {
        var events: MutableList<SseEvent>? = null
        var start = offset
        val end = offset + length
        var i = offset
        while (i < end) {
            if (bytes[i] == '\n'.code.toByte()) {
                line.write(bytes, start, i - start)
                checkSize()
                var text = line.readUtf8()
                if (text.endsWith('\r')) text = text.dropLast(1)
                process(text)?.let { (events ?: mutableListOf<SseEvent>().also { events = it }).add(it) }
                start = i + 1
            }
            i++
        }
        line.write(bytes, start, end - start)
        checkSize()
        return events ?: emptyList()
    }

    private fun checkSize() {
        if (line.size + dataBytes > maxEventBytes) throw SseEventTooLargeException(maxEventBytes)
    }

    /** Applies one complete line; an empty line dispatches the pending event. */
    private fun process(text: String): SseEvent? {
        if (text.isEmpty()) {
            val name = event
            val had = hasData
            val value = data.toString()
            event = null
            data.setLength(0)
            dataBytes = 0
            hasData = false
            if (name == null && !had) return null
            return SseEvent(name ?: "message", value)
        }
        if (text.startsWith(':')) return null
        val colon = text.indexOf(':')
        val field = if (colon < 0) text else text.substring(0, colon)
        val value = if (colon < 0) "" else text.substring(colon + 1).removePrefix(" ")
        when (field) {
            "event" -> event = value
            "data" -> {
                if (hasData) {
                    data.append('\n')
                    dataBytes += 1
                }
                data.append(value)
                dataBytes += utf8Length(value)
                hasData = true
                if (dataBytes > maxEventBytes) throw SseEventTooLargeException(maxEventBytes)
            }
            else -> Unit
        }
        return null
    }

    private fun utf8Length(s: String): Long {
        var n = 0L
        var i = 0
        while (i < s.length) {
            val c = s[i]
            n += when {
                c.code < 0x80 -> 1
                c.code < 0x800 -> 2
                Character.isHighSurrogate(c) && i + 1 < s.length && Character.isLowSurrogate(s[i + 1]) -> {
                    i++
                    4
                }
                else -> 3
            }
            i++
        }
        return n
    }

    companion object {
        /** 4 MiB: far above any real event, which carries one flag key. */
        const val MAX_EVENT_BYTES = 4 * 1024 * 1024
    }
}

/**
 * The stream's timings. The client uses [DEFAULT]; tests pass milliseconds so nothing waits for
 * seconds.
 */
internal data class RealtimeTimings(
    /** The first retry's full delay; the wait is half of it to all of it. */
    val baseDelayMillis: Long = 1_000L,
    /** Caps the delay after a dropped stream or a transport error. */
    val maxDelayMillis: Long = 30_000L,
    /**
     * Caps the delay after a 401, 403 or 404: the key is refused or the endpoint absent, and
     * retrying fast changes nothing but the battery.
     */
    val rejectedMaxDelayMillis: Long = 300_000L,
    /** Caps how far a Retry-After can stretch one wait, so a proxy cannot hold off for hours. */
    val maxRetryAfterMillis: Long = 60_000L,
    /** How long a connection must stay up to reset the backoff. */
    val stableAfterMillis: Long = 30_000L,
    /**
     * The longest extra wait before the first retry of a stream that was up, so the clients
     * whose streams one deploy dropped do not reconnect together.
     */
    val spreadMillis: Long = 5_000L,
    /**
     * The socket read timeout: it bounds the wait for the response headers and drops a
     * connection that received nothing for this long. The server sends a heartbeat every 30 s;
     * three missed ones mean the connection is dead even if the socket has not noticed (a phone
     * that changed networks, say).
     */
    val readTimeoutMillis: Long = 90_000L,
    /** Returns a value in [0, 1) for jitter. Must be safe for concurrent use. */
    val random: () -> Double = { Random.nextDouble() },
) {
    /**
     * The wait after a connection that ended as [end] before attempt [attempt] (from 0).
     * [status] and [retryAfterMillis] (-1 when absent) are those of a refused attempt.
     */
    fun nextDelay(end: StreamEnd, status: Int, retryAfterMillis: Long, attempt: Int): Long {
        val ceiling = if (end == StreamEnd.REFUSED && (status == 401 || status == 403 || status == 404)) {
            rejectedMaxDelayMillis
        } else {
            maxDelayMillis
        }
        var delay = RealtimeStream.reconnectDelay(baseDelayMillis, attempt, ceiling, random())
        delay = RealtimeStream.saturatedAdd(
            delay,
            RealtimeStream.reconnectSpread(spreadMillis, attempt, end == StreamEnd.DROPPED, random()),
        )
        if (end == StreamEnd.REFUSED && retryAfterMillis >= 0 && (status == 429 || status == 503)) {
            // Retry-After only ever lengthens the wait.
            delay = maxOf(delay, minOf(retryAfterMillis, maxRetryAfterMillis))
        }
        return delay
    }

    companion object {
        val DEFAULT = RealtimeTimings()
    }
}

/** How one connection attempt ended. */
internal enum class StreamEnd {
    /** The request failed before a response (refused, unreachable, timeout). */
    UNREACHABLE,
    /** A non-2xx status (a redirect included), or a 2xx that is not `text/event-stream`. */
    REFUSED,
    /** The stream was open, then ended (closed, broke, too large, or went idle). */
    DROPPED,
}

/**
 * The flag-change stream, `GET /api/flags/stream` (docs/SDK-DEVELOPER-GUIDE.md, Real-time
 * Updates), which invalidates cached evaluations so a flag change, a kill switch above all,
 * reaches the app in about a round trip instead of at the next poll (#167).
 *
 * One long-lived connection per client, read in [Dispatchers.IO] by a coroutine on the client's
 * scope, until that coroutine is cancelled ([OttoFlagsClient.close] cancels the scope; the
 * blocking read is unblocked by cancelling the OkHttp [Call]). A `flag.created`, `flag.updated`
 * or `flag.deleted` event invalidates that flag through the client's generation counters, so an
 * evaluation in flight when it arrived is not cached either (#137). Each `connected` event (the
 * server sends one on every connect) invalidates the whole cache: events may have been missed
 * while the stream was down. Lookups never wait on the stream: it only invalidates, and polling
 * keeps running as the fallback.
 *
 * A dropped or refused stream is retried with exponential backoff and jitter
 * ([RealtimeTimings.nextDelay]); the attempt count resets only after a connection stayed up for
 * [RealtimeTimings.stableAfterMillis], so a server that accepts and drops at once is not retried
 * at the base delay forever. The first failed attempt of each run of failures is logged through
 * [logError] (a run ends when a connection opens), so a long outage does not flood the log.
 * Nothing is thrown into the host. The key is sent only in the `Authorization` header and is
 * never put in a log line.
 */
internal class RealtimeStream(
    url: HttpUrl,
    sdkKey: String,
    baseClient: OkHttpClient,
    private val invalidate: (String?) -> Unit,
    private val logError: (String, Throwable?) -> Unit,
    private val timings: RealtimeTimings = RealtimeTimings.DEFAULT,
) {
    private val request: Request = Request.Builder()
        .url(url)
        .get()
        .header("Authorization", "Bearer $sdkKey")
        .header("Accept", "text/event-stream")
        // Transparent gzip could hold events back behind a proxy that compresses without
        // flushing each one.
        .header("Accept-Encoding", "identity")
        .build()

    private val client: OkHttpClient = baseClient.newBuilder()
        .apply {
            // The client's debug logging interceptor would print the Authorization header,
            // and at BODY level would try to buffer a body that never ends.
            interceptors().clear()
            networkInterceptors().clear()
        }
        .readTimeout(timings.readTimeoutMillis, TimeUnit.MILLISECONDS)
        // No total timeout, which would cut the stream off.
        .callTimeout(0L, TimeUnit.MILLISECONDS)
        // A redirect could carry the key elsewhere; a 3xx is a refused attempt.
        .followRedirects(false)
        .followSslRedirects(false)
        .build()

    /**
     * Launches the stream on [scope]; cancelling the returned job (or the scope) stops it. The
     * handler keeps anything that still escapes (an Error) away from the thread's uncaught
     * exception handler, which on Android would crash the app.
     */
    fun start(scope: CoroutineScope): Job =
        scope.launch(CoroutineExceptionHandler { _, e -> logError("Otto Flags stream: stopped", e) }) { run() }

    private suspend fun run() = coroutineScope {
        var attempt = 0
        var failureLogged = false
        while (isActive) {
            val outcome = connectOnce()
            if (!isActive) break
            if (outcome.end == StreamEnd.DROPPED &&
                TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - outcome.openedAtNanos) >= timings.stableAfterMillis
            ) {
                attempt = 0
            }
            val wait = timings.nextDelay(outcome.end, outcome.status, outcome.retryAfterMillis, attempt)
            if (attempt < MAX_ATTEMPT) attempt++
            if (outcome.end == StreamEnd.DROPPED) {
                failureLogged = false
            } else if (!failureLogged) {
                failureLogged = true
                logError(failureMessage(outcome) + "; retrying in $wait ms", outcome.cause)
            }
            delay(wait)
        }
    }

    private class Outcome(
        val end: StreamEnd,
        val status: Int = 0,
        val retryAfterMillis: Long = -1L,
        val openedAtNanos: Long = 0L,
        val cause: Throwable? = null,
    )

    private fun failureMessage(outcome: Outcome): String = when {
        outcome.end == StreamEnd.UNREACHABLE ->
            "Otto Flags stream: connect failed: ${outcome.cause?.javaClass?.simpleName ?: "unknown error"}"
        outcome.status in 200..299 ->
            "Otto Flags stream: the response is not text/event-stream (status ${outcome.status})"
        else -> "Otto Flags stream: connect refused (status ${outcome.status})"
    }

    /**
     * One connection attempt, read to its end. The blocking read runs in a child on
     * [Dispatchers.IO]; when this coroutine is cancelled the call is cancelled, which makes the
     * read throw and the child finish.
     */
    private suspend fun connectOnce(): Outcome = coroutineScope {
        val call = client.newCall(request)
        val reader = async(Dispatchers.IO) { read(call) }
        try {
            reader.await()
        } catch (e: CancellationException) {
            call.cancel()
            throw e
        }
    }

    private fun read(call: Call): Outcome {
        var openedAt = 0L
        try {
            call.execute().use { response ->
                if (!response.isSuccessful) {
                    return Outcome(
                        StreamEnd.REFUSED,
                        response.code,
                        parseRetryAfter(response.header("Retry-After")),
                    )
                }
                val body = response.body
                val type = body?.contentType()
                if (body == null || type == null || type.type != "text" || type.subtype != "event-stream") {
                    return Outcome(StreamEnd.REFUSED, response.code)
                }
                openedAt = System.nanoTime()
                val parser = SseParser()
                val source = body.source()
                val chunk = ByteArray(8 * 1024)
                while (true) {
                    val n = source.read(chunk)
                    if (n == -1) break
                    for (event in parser.feed(chunk, 0, n)) {
                        if (call.isCanceled()) break
                        try {
                            applyEvent(event, invalidate)
                        } catch (e: Exception) {
                            logError("Otto Flags stream: applying an event failed", e)
                        } catch (e: StackOverflowError) {
                            // Deeply nested JSON in an event's data.
                            logError("Otto Flags stream: applying an event failed", e)
                        }
                    }
                }
                return Outcome(StreamEnd.DROPPED, openedAtNanos = openedAt)
            }
        } catch (e: IOException) {
            return if (openedAt != 0L) {
                Outcome(StreamEnd.DROPPED, openedAtNanos = openedAt, cause = e)
            } else {
                Outcome(StreamEnd.UNREACHABLE, cause = e)
            }
        } catch (e: RuntimeException) {
            // An unexpected failure inside OkHttp must not end the stream coroutine for good.
            return if (openedAt != 0L) {
                Outcome(StreamEnd.DROPPED, openedAtNanos = openedAt, cause = e)
            } else {
                Outcome(StreamEnd.UNREACHABLE, cause = e)
            }
        }
    }

    companion object {
        /** Bounds the attempt counter; the delay reaches its cap long before. */
        const val MAX_ATTEMPT = 64

        /**
         * `<apiUrl>/api/flags/stream`, keeping any path prefix in [apiUrl] (a trailing slash is
         * absorbed) and dropping any query or fragment; null when [apiUrl] is not an http(s) URL.
         */
        fun streamUrl(apiUrl: String): HttpUrl? {
            val base = apiUrl.toHttpUrlOrNull() ?: return null
            // An empty last segment (a trailing slash) is replaced by the first added one.
            return base.newBuilder()
                .query(null)
                .fragment(null)
                .addPathSegment("api")
                .addPathSegment("flags")
                .addPathSegment("stream")
                .build()
        }

        /**
         * Applies one event. `heartbeat` and events this SDK does not know are ignored. A flag
         * event can never clear the whole cache: only `connected` passes null.
         */
        fun applyEvent(event: SseEvent, invalidate: (String?) -> Unit) {
            when (event.event) {
                "connected" -> invalidate(null)
                "flag.created", "flag.updated", "flag.deleted" ->
                    changedFlagKey(event.data)?.let { invalidate(it) }
                else -> Unit
            }
        }

        /**
         * The key of a flag event, if its data is a JSON object with a string `key` that a flag
         * can have; else null. Event text is untrusted: it is only ever used as a cache key, and
         * only after [OttoFlagsClient.isValidFlagKey].
         */
        fun changedFlagKey(data: String): String? {
            val key = try {
                JSONObject(data).opt("key") as? String
            } catch (e: Exception) {
                // org.json.JSONException (malformed or not an object), and anything else.
                null
            } ?: return null
            return if (OttoFlagsClient.isValidFlagKey(key)) key else null
        }

        /** Maps [unit] into [0, 1]; NaN is 0. */
        private fun clampUnit(unit: Double): Double =
            if (unit.isNaN() || unit < 0) 0.0 else minOf(unit, 1.0)

        /**
         * The wait before reconnect attempt [attempt] (from 0): `min(base * 2^attempt, ceiling)`,
         * scaled into [50%, 100%] by [unit] in [0, 1), so clients dropped together do not
         * reconnect together. A unit out of range is clamped.
         */
        fun reconnectDelay(baseMillis: Long, attempt: Int, ceilingMillis: Long, unit: Double): Long {
            if (baseMillis <= 0 || ceilingMillis <= 0) return 0
            var d = baseMillis
            var i = 0
            while (i < attempt && d < ceilingMillis) {
                d = if (d > Long.MAX_VALUE / 2) Long.MAX_VALUE else d * 2
                i++
            }
            d = minOf(d, ceilingMillis)
            return (d * (0.5 + clampUnit(unit) / 2)).toLong()
        }

        /**
         * The extra wait on top of [reconnectDelay] for the first retry after an established
         * stream ended, uniform in [0, spread) by [unit]; zero otherwise.
         */
        fun reconnectSpread(spreadMillis: Long, attempt: Int, wasUp: Boolean, unit: Double): Long {
            if (attempt != 0 || !wasUp || spreadMillis <= 0 || unit.isNaN()) return 0
            return (spreadMillis * clampUnit(unit)).toLong()
        }

        /**
         * A Retry-After header given in seconds, in milliseconds; -1 when absent or not a plain
         * non-negative integer (an HTTP date, a negative or fractional value). A value too large
         * for a Long saturates.
         */
        fun parseRetryAfter(value: String?): Long {
            val trimmed = value?.trim() ?: return -1L
            if (trimmed.isEmpty() || !trimmed.all { it in '0'..'9' }) return -1L
            val seconds = trimmed.toLongOrNull() ?: return Long.MAX_VALUE
            return if (seconds > Long.MAX_VALUE / 1000) Long.MAX_VALUE else seconds * 1000
        }

        fun saturatedAdd(a: Long, b: Long): Long {
            val sum = a + b
            return if (sum < 0) Long.MAX_VALUE else sum
        }
    }
}
