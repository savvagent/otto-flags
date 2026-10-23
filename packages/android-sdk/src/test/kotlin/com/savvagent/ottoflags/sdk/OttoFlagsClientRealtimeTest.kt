package com.savvagent.ottoflags.sdk

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.Interceptor
import okhttp3.OkHttpClient
import okhttp3.mockwebserver.Dispatcher
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import java.net.ServerSocket
import java.net.SocketTimeoutException
import java.util.Collections
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * The flag-change stream (#167). The mock routes by path: the stream path answers from
 * [streamResponses] (or [streamFallback] when that is empty, else it holds the request until a
 * response is queued), evaluate answers `true`, and the flag list lists `checkout`.
 *
 * These tests use real time ([runBlocking], short [timings]): the stream runs on real threads.
 */
class OttoFlagsClientRealtimeTest {

    private lateinit var server: MockWebServer
    private val clients = mutableListOf<OttoFlagsClient>()
    private val streamResponses = LinkedBlockingQueue<MockResponse>()
    @Volatile private var streamFallback: MockResponse? = null
    @Volatile private var shuttingDown = false
    private val streamRequests = Collections.synchronizedList(mutableListOf<RecordedRequest>())
    private val evaluations = AtomicInteger()
    private val flagLists = AtomicInteger()
    private val otherRequests = AtomicInteger()

    private val timings = RealtimeTimings(
        baseDelayMillis = 20,
        maxDelayMillis = 40,
        rejectedMaxDelayMillis = 40,
        maxRetryAfterMillis = 40,
        stableAfterMillis = 10_000,
        spreadMillis = 0,
        readTimeoutMillis = 5_000,
    )

    private val user = UserContext(userId = "user-1")

    @Before
    fun setUp() {
        server = MockWebServer()
        server.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val path = request.requestUrl?.encodedPath ?: ""
                return when {
                    path.endsWith("/api/flags/stream") -> {
                        streamRequests.add(request)
                        nextStreamResponse()
                    }
                    path.endsWith("/evaluate") -> {
                        evaluations.incrementAndGet()
                        MockResponse().setBody("""{"enabled": true}""")
                    }
                    path.endsWith("/api/sdk/flags") -> {
                        flagLists.incrementAndGet()
                        MockResponse().setBody("""{"flags": [{"key": "checkout"}]}""")
                    }
                    else -> {
                        otherRequests.incrementAndGet()
                        MockResponse().setResponseCode(404)
                    }
                }
            }
        }
        server.start()
    }

    private fun nextStreamResponse(): MockResponse {
        while (!shuttingDown) {
            streamResponses.poll(20, TimeUnit.MILLISECONDS)?.let { return it }
            streamFallback?.let { return it }
        }
        return MockResponse().setResponseCode(503)
    }

    @After
    fun tearDown() {
        clients.forEach { it.close() }
        shuttingDown = true
        server.shutdown()
    }

    private fun apiUrl() = server.url("/").toString().removeSuffix("/")

    private fun client(
        enableRealtime: Boolean = true,
        pollingInterval: Long = 0,
        apiUrl: String = apiUrl(),
    ): OttoFlagsClient {
        val config = OttoFlagsConfig(
            apiUrl = apiUrl,
            sdkKey = "sdk_test-key",
            environment = "test",
            pollingInterval = pollingInterval,
            enableLogging = true,
            enableRealtime = enableRealtime,
        )
        return OttoFlagsClient(config, null, timings).also { clients.add(it) }
    }

    private fun events(vararg lines: String): MockResponse =
        MockResponse()
            .addHeader("Content-Type", "text/event-stream")
            .setBody(lines.joinToString(""))

    private fun event(name: String, data: String) = "event: $name\ndata: $data\n\n"

    private fun waitFor(what: String, timeoutMillis: Long = 5_000, condition: () -> Boolean) {
        val deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(timeoutMillis)
        while (!condition()) {
            if (System.nanoTime() > deadline) fail("timed out waiting for $what")
            Thread.sleep(5)
        }
    }

    private fun cache(client: OttoFlagsClient, vararg keys: String) = runBlocking {
        for (key in keys) {
            client.evaluate(key, user).getOrThrow()
            assertTrue("$key cached", client.isCached(key))
        }
    }

    // --- the client and the stream ---

    @Test
    fun testFlagUpdatedDropsTheCachedEvaluation() {
        val client = client()
        waitFor("the stream request") { streamRequests.size == 1 }
        cache(client, "checkout", "search")

        streamResponses.add(events(event("flag.updated", """{"key":"checkout"}""")))

        waitFor("checkout invalidated") { !client.isCached("checkout") }
        assertTrue(client.isCached("search"))
        val again = runBlocking { client.evaluate("checkout", user).getOrThrow() }
        assertEquals("evaluated", again.reason)
    }

    @Test
    fun testCreatedAndDeletedEventsInvalidateToo() {
        val client = client()
        waitFor("the stream request") { streamRequests.size == 1 }
        cache(client, "a", "b")

        streamResponses.add(
            events(event("flag.created", """{"key":"a"}"""), event("flag.deleted", """{"key":"b"}"""))
        )

        waitFor("both invalidated") { !client.isCached("a") && !client.isCached("b") }
    }

    @Test
    fun testOtherFlagsInvalidKeysAndUnknownEventsLeaveTheCacheAlone() {
        val client = client()
        waitFor("the stream request") { streamRequests.size == 1 }
        cache(client, "checkout", "fence")

        streamResponses.add(
            events(
                event("flag.updated", """{"key":"other"}"""),
                event("flag.updated", """{"key":"../checkout"}"""),
                event("flag.updated", """{"key":""}"""),
                event("flag.updated", """{"key":42}"""),
                event("flag.updated", "not json"),
                event("flag.updated", """["checkout"]"""),
                // Deeply nested JSON: a StackOverflowError while parsing must not end the stream.
                event("flag.updated", "{\"a\":".repeat(100_000)),
                event("heartbeat", "{}"),
                event("mystery", """{"key":"checkout"}"""),
                "data: {\"key\":\"checkout\"}\n\n",
                ": comment\n\n",
                // Events are applied in order: once the fence is gone, the rest were seen.
                event("flag.updated", """{"key":"fence"}"""),
            )
        )

        waitFor("the fence invalidated") { !client.isCached("fence") }
        assertTrue(client.isCached("checkout"))
    }

    @Test
    fun testADroppedStreamReconnectsAndConnectedClearsTheCache() {
        val client = client()
        waitFor("the stream request") { streamRequests.size == 1 }
        cache(client, "checkout", "search")

        // The first stream ends at once; the client reconnects and is held there.
        streamResponses.add(events(": hello\n\n"))
        waitFor("a reconnect") { streamRequests.size == 2 }
        assertTrue(client.isCached("checkout"))

        streamResponses.add(events(event("connected", "{}")))
        waitFor("the cache cleared") { !client.isCached("checkout") && !client.isCached("search") }
    }

    @Test
    fun testWhileTheStreamIsRefusedLookupsWorkAndThePollStillRefreshes() {
        streamFallback = MockResponse().setResponseCode(503).addHeader("Retry-After", "1")
        val client = client(pollingInterval = 100)
        waitFor("stream retries") { streamRequests.size >= 3 }

        cache(client, "checkout")
        waitFor("the poll invalidating checkout") { !client.isCached("checkout") }
        assertTrue(flagLists.get() >= 1)
        val again = runBlocking { client.evaluate("checkout", user).getOrThrow() }
        assertTrue(again.value)
    }

    @Test
    fun testAStreamThatIsNotAnEventStreamIsRetried() {
        streamFallback = MockResponse().setBody("{}").addHeader("Content-Type", "application/json")
        val client = client()
        waitFor("stream retries") { streamRequests.size >= 3 }
        cache(client, "checkout")
    }

    @Test
    fun testNoStreamRequestWhenRealtimeIsOff() {
        assertFalse(OttoFlagsConfig(sdkKey = "k").enableRealtime)
        val client = OttoFlagsClient(
            OttoFlagsConfig(apiUrl = apiUrl(), sdkKey = "sdk_test-key", pollingInterval = 0)
        ).also { clients.add(it) }
        cache(client, "checkout")
        Thread.sleep(300)

        assertEquals(0, streamRequests.size)
        assertEquals(1, server.requestCount)
    }

    @Test
    fun testALookupIsPromptWhileTheStreamIsHeld() {
        // Headers sent, then nothing for a while.
        streamResponses.add(events(event("connected", "{}")).setBodyDelay(2, TimeUnit.SECONDS))
        val client = client()
        waitFor("the stream request") { streamRequests.size == 1 }

        val started = System.nanoTime()
        val result = runBlocking { client.evaluate("checkout", user).getOrThrow() }
        val tookMillis = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - started)

        assertTrue(result.value)
        assertTrue("lookup took $tookMillis ms", tookMillis < 1_000)
    }

    @Test
    fun testTheStreamRequestCarriesTheKeyInTheHeaderAndNoQuery() {
        client(apiUrl = apiUrl() + "/prefix/?token=secret#frag")
        waitFor("the stream request") { streamRequests.size == 1 }

        val request = streamRequests[0]
        assertEquals("GET", request.method)
        assertEquals("/prefix/api/flags/stream", request.path)
        assertEquals("Bearer sdk_test-key", request.getHeader("Authorization"))
        assertEquals("text/event-stream", request.getHeader("Accept"))
        assertEquals("identity", request.getHeader("Accept-Encoding"))
        assertNull(request.requestUrl?.query)
    }

    @Test
    fun testARedirectIsNotFollowed() {
        streamFallback = MockResponse()
            .setResponseCode(307)
            .addHeader("Location", server.url("/elsewhere").toString())
        client()
        waitFor("stream retries") { streamRequests.size >= 3 }

        assertEquals(0, otherRequests.get())
    }

    @Test
    fun testCloseStopsReconnecting() {
        streamFallback = MockResponse().setResponseCode(503)
        val client = client()
        waitFor("stream retries") { streamRequests.size >= 2 }

        client.close()
        Thread.sleep(100)
        val after = streamRequests.size
        Thread.sleep(300)

        assertEquals(after, streamRequests.size)
    }

    @Test
    fun testCloseEndsAHeldStream() {
        streamResponses.add(events(event("connected", "{}")).setBodyDelay(3, TimeUnit.SECONDS))
        val client = client()
        waitFor("the stream request") { streamRequests.size == 1 }

        client.close()
        Thread.sleep(300)

        assertEquals(1, streamRequests.size)
    }

    @Test
    fun testAnUnusableApiUrlStartsNoStream() {
        // Not an http(s) URL: evaluate fails and the stream never starts; nothing throws.
        val client = client(apiUrl = "ftp://example.invalid")
        val result = runBlocking { client.evaluate("checkout", user) }

        assertTrue(result.isFailure)
        assertNull(RealtimeStream.streamUrl("ftp://example.invalid"))
        assertEquals(0, server.requestCount)
    }

    // --- RealtimeStream on its own ---

    @Test
    fun testFailuresAreLoggedOncePerRunWithoutTheKey() {
        streamFallback = MockResponse().setResponseCode(401)
        val logged = Collections.synchronizedList(mutableListOf<String>())
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        try {
            RealtimeStream(
                url = RealtimeStream.streamUrl(apiUrl())!!,
                sdkKey = "sdk_secret-key",
                baseClient = OkHttpClient(),
                invalidate = {},
                logError = { message, _ -> logged.add(message) },
                timings = timings,
            ).start(scope)
            waitFor("stream retries") { streamRequests.size >= 4 }
        } finally {
            scope.cancel()
        }

        assertEquals(logged.toString(), 1, logged.size)
        assertTrue(logged[0], logged[0].contains("status 401"))
        assertFalse(logged[0].contains("secret"))
    }

    @Test
    fun testAFailingInvalidateDoesNotEndTheStream() {
        val seen = Collections.synchronizedList(mutableListOf<String?>())
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        streamResponses.add(
            events(
                event("flag.updated", """{"key":"boom"}"""),
                event("flag.updated", """{"key":"overflow"}"""),
                event("flag.updated", """{"key":"after"}"""),
            )
        )
        try {
            RealtimeStream(
                url = RealtimeStream.streamUrl(apiUrl())!!,
                sdkKey = "sdk_test-key",
                baseClient = OkHttpClient(),
                invalidate = { key ->
                    seen.add(key)
                    when (key) {
                        "boom" -> throw IllegalStateException("boom")
                        "overflow" -> throw StackOverflowError()
                    }
                },
                logError = { _, _ -> },
                timings = timings,
            ).start(scope)
            waitFor("the last event") { seen.contains("after") }
        } finally {
            scope.cancel()
        }
        assertEquals(listOf("boom", "overflow", "after"), seen.toList())
    }

    @Test
    fun testCloseCancelsTheOpenConnection() {
        // A raw socket server that answers the stream request with headers and then holds the
        // body, so only a cancelled call can end the connection before the read timeout.
        ServerSocket(0, 1, java.net.InetAddress.getByName("127.0.0.1")).use { raw ->
            val accepted = java.util.concurrent.CountDownLatch(1)
            val closedByClient = java.util.concurrent.atomic.AtomicReference<String?>(null)
            val serverThread = Thread {
                try {
                    raw.accept().use { socket ->
                        val input = socket.getInputStream()
                        // Read the request head.
                        val head = StringBuilder()
                        while (!head.endsWith("\r\n\r\n")) {
                            val b = input.read()
                            if (b < 0) return@Thread
                            head.append(b.toChar())
                        }
                        socket.getOutputStream().apply {
                            write(
                                ("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n" +
                                    "Transfer-Encoding: chunked\r\n\r\n").toByteArray()
                            )
                            flush()
                        }
                        accepted.countDown()
                        socket.soTimeout = 5_000
                        val started = System.nanoTime()
                        try {
                            val r = input.read()
                            closedByClient.set(
                                if (r == -1) "eof after ${TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - started)} ms"
                                else "unexpected byte $r"
                            )
                        } catch (e: SocketTimeoutException) {
                            closedByClient.set(null)
                        } catch (e: java.io.IOException) {
                            // A reset is also the client dropping the connection.
                            closedByClient.set("reset after ${TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - started)} ms")
                        }
                    }
                } catch (e: java.io.IOException) {
                    // test teardown
                }
            }.apply { isDaemon = true; start() }

            val client = client(apiUrl = "http://127.0.0.1:${raw.localPort}")
            assertTrue("the stream connected", accepted.await(5, TimeUnit.SECONDS))
            // Let the client settle into the blocked body read.
            Thread.sleep(100)

            val closeStarted = System.nanoTime()
            client.close()
            serverThread.join(1_000)
            val elapsed = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - closeStarted)

            assertNotNull("the client socket stayed open after close() (${elapsed} ms)", closedByClient.get())
            assertTrue("closed after $elapsed ms", elapsed < 1_500)
        }
    }

    @Test
    fun testTheStreamClientRunsNoInterceptorsOfTheBaseClient() {
        val applicationSeen = AtomicInteger()
        val networkSeen = AtomicInteger()
        val base = OkHttpClient.Builder()
            .addInterceptor(Interceptor { chain -> applicationSeen.incrementAndGet(); chain.proceed(chain.request()) })
            .addNetworkInterceptor(Interceptor { chain -> networkSeen.incrementAndGet(); chain.proceed(chain.request()) })
            .build()
        streamFallback = MockResponse().setResponseCode(503)

        // Control: the base client does run its interceptors.
        base.newCall(okhttp3.Request.Builder().url(server.url("/control")).build()).execute().close()
        assertEquals(1, applicationSeen.get())
        assertEquals(1, networkSeen.get())

        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        try {
            RealtimeStream(
                url = RealtimeStream.streamUrl(apiUrl())!!,
                sdkKey = "sdk_test-key",
                baseClient = base,
                invalidate = {},
                logError = { _, _ -> },
                timings = timings,
            ).start(scope)
            waitFor("stream retries") { streamRequests.size >= 2 }
        } finally {
            scope.cancel()
        }

        assertEquals(1, applicationSeen.get())
        assertEquals(1, networkSeen.get())
    }

    // --- pure functions ---

    @Test
    fun testParserSplitsEventsAcrossEveryChunkBoundary() {
        val body = (
            ": heartbeat\n" +
                "event: connected\r\n" +
                "data: {}\r\n" +
                "\r\n" +
                "event: flag.updated\n" +
                "data: {\"key\":\n" +
                "data:\"café\"}\n" +
                "id: 7\n" +
                "retry: 10\n" +
                "\n" +
                "data\n" +
                "\n" +
                "event: tail\n"
            ).toByteArray(Charsets.UTF_8)
        val expected = listOf(
            SseEvent("connected", "{}"),
            SseEvent("flag.updated", "{\"key\":\n\"café\"}"),
            SseEvent("message", ""),
        )

        for (split in 0..body.size) {
            val parser = SseParser()
            val got = parser.feed(body, 0, split) + parser.feed(body, split, body.size - split)
            assertEquals("split at $split", expected, got)
        }
        val parser = SseParser()
        val byteByByte = body.indices.flatMap { parser.feed(body, it, 1) }
        assertEquals(expected, byteByByte)
    }

    @Test
    fun testParserDropsAnEventWithNoFields() {
        val parser = SseParser()
        assertEquals(emptyList<SseEvent>(), parser.feed("\n\n: x\n\nid: 1\n\n".toByteArray()))
        assertEquals(listOf(SseEvent("e", "")), parser.feed("event: e\n\n".toByteArray()))
        assertEquals(listOf(SseEvent("message", "a:b")), parser.feed("data:a:b\n\n".toByteArray()))
    }

    @Test
    fun testParserRefusesAnEventOverTheCap() {
        val parser = SseParser(maxEventBytes = 16)
        parser.feed("data: 0123456789\n".toByteArray())
        assertThrows(SseEventTooLargeException::class.java) {
            parser.feed("data: 0123456789\n".toByteArray())
        }
        val unterminated = SseParser(maxEventBytes = 16)
        assertThrows(SseEventTooLargeException::class.java) {
            unterminated.feed("data: ${"x".repeat(32)}".toByteArray())
        }
        // Empty data lines cost a byte each.
        val empties = SseParser(maxEventBytes = 16)
        assertThrows(SseEventTooLargeException::class.java) {
            empties.feed("data:\n".repeat(40).toByteArray())
        }
        // A dispatched event frees its budget.
        val reused = SseParser(maxEventBytes = 16)
        repeat(10) { assertEquals(1, reused.feed("data: 0123456789\n\n".toByteArray()).size) }
    }

    @Test
    fun testReconnectDelayDoublesToTheCapWithJitterInTheUpperHalf() {
        assertEquals(500, RealtimeStream.reconnectDelay(1_000, 0, 30_000, 0.0))
        assertEquals(1_000, RealtimeStream.reconnectDelay(1_000, 0, 30_000, 1.0))
        assertEquals(7_996, RealtimeStream.reconnectDelay(1_000, 3, 30_000, 0.999))
        assertEquals(2_000, RealtimeStream.reconnectDelay(1_000, 2, 30_000, 0.0))
        assertEquals(15_000, RealtimeStream.reconnectDelay(1_000, 64, 30_000, 0.0))
        assertEquals(30_000, RealtimeStream.reconnectDelay(1_000, Int.MAX_VALUE, 30_000, 1.0))
        for (attempt in 0..70) {
            for (unit in listOf(0.0, 0.25, 0.5, 0.999, -1.0, 2.0, Double.NaN)) {
                val d = RealtimeStream.reconnectDelay(1_000, attempt, 30_000, unit)
                assertTrue("attempt $attempt unit $unit: $d", d in 500..30_000)
            }
        }
        assertEquals(0, RealtimeStream.reconnectDelay(0, 3, 30_000, 0.5))
    }

    @Test
    fun testReconnectSpreadAppliesOnlyToTheFirstRetryOfAnEstablishedStream() {
        assertEquals(2_500, RealtimeStream.reconnectSpread(5_000, 0, true, 0.5))
        assertEquals(0, RealtimeStream.reconnectSpread(5_000, 1, true, 0.5))
        assertEquals(0, RealtimeStream.reconnectSpread(5_000, 0, false, 0.5))
        assertEquals(0, RealtimeStream.reconnectSpread(5_000, 0, true, Double.NaN))
        assertEquals(5_000, RealtimeStream.reconnectSpread(5_000, 0, true, 7.0))
    }

    @Test
    fun testNextDelayCapsRejectionsLongerAndHonoursRetryAfter() {
        val t = RealtimeTimings(random = { 1.0 })
        assertEquals(30_000, t.nextDelay(StreamEnd.REFUSED, 500, -1, 20))
        assertEquals(300_000, t.nextDelay(StreamEnd.REFUSED, 401, -1, 20))
        assertEquals(300_000, t.nextDelay(StreamEnd.REFUSED, 403, -1, 20))
        assertEquals(300_000, t.nextDelay(StreamEnd.REFUSED, 404, -1, 20))
        // Retry-After lengthens, is capped, and never shortens.
        assertEquals(42_000, t.nextDelay(StreamEnd.REFUSED, 503, 42_000, 0))
        assertEquals(60_000, t.nextDelay(StreamEnd.REFUSED, 429, 3_600_000, 0))
        assertEquals(30_000, t.nextDelay(StreamEnd.REFUSED, 503, 1_000, 20))
        assertEquals(1_000, t.nextDelay(StreamEnd.REFUSED, 500, 42_000, 0))
        // The first retry after an established stream adds the spread.
        assertEquals(6_000, t.nextDelay(StreamEnd.DROPPED, 0, -1, 0))
        assertEquals(1_000, t.nextDelay(StreamEnd.UNREACHABLE, 0, -1, 0))
    }

    @Test
    fun testParseRetryAfter() {
        assertEquals(-1, RealtimeStream.parseRetryAfter(null))
        assertEquals(-1, RealtimeStream.parseRetryAfter(""))
        assertEquals(-1, RealtimeStream.parseRetryAfter("-1"))
        assertEquals(-1, RealtimeStream.parseRetryAfter("1.5"))
        assertEquals(-1, RealtimeStream.parseRetryAfter("Wed, 21 Oct 2026 07:28:00 GMT"))
        assertEquals(0, RealtimeStream.parseRetryAfter("0"))
        assertEquals(120_000, RealtimeStream.parseRetryAfter(" 120 "))
        assertEquals(Long.MAX_VALUE, RealtimeStream.parseRetryAfter("99999999999999999999999"))
    }

    @Test
    fun testStreamUrlKeepsAPathPrefixAndDropsTheQuery() {
        assertEquals("https://h.example/api/flags/stream", RealtimeStream.streamUrl("https://h.example").toString())
        assertEquals("https://h.example/api/flags/stream", RealtimeStream.streamUrl("https://h.example/").toString())
        assertEquals(
            "https://h.example/p/api/flags/stream",
            RealtimeStream.streamUrl("https://h.example/p/?a=b#c").toString(),
        )
        assertNull(RealtimeStream.streamUrl("not a url"))
        assertNull(RealtimeStream.streamUrl(""))
        assertEquals("http://h.example:8080/api/flags/stream".toHttpUrl(), RealtimeStream.streamUrl("http://h.example:8080"))
    }

    @Test
    fun testApplyEventMapsEventsToInvalidations() {
        fun applied(name: String, data: String): List<String?> {
            val out = mutableListOf<String?>()
            RealtimeStream.applyEvent(SseEvent(name, data)) { out.add(it) }
            return out
        }
        assertEquals(listOf<String?>(null), applied("connected", ""))
        assertEquals(listOf<String?>("a.b-c_1"), applied("flag.updated", """{"key":"a.b-c_1","version":3}"""))
        assertEquals(emptyList<String?>(), applied("flag.updated", """{"key":".."}"""))
        assertEquals(emptyList<String?>(), applied("flag.updated", """{"key":"a/b"}"""))
        assertEquals(emptyList<String?>(), applied("flag.updated", """{"key":null}"""))
        assertEquals(emptyList<String?>(), applied("flag.updated", """{"key":"${"k".repeat(129)}"}"""))
        assertEquals(emptyList<String?>(), applied("heartbeat", "{}"))
        assertEquals(emptyList<String?>(), applied("message", """{"key":"a"}"""))
    }
}
