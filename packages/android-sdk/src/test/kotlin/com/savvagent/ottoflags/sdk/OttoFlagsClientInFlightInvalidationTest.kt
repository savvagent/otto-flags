package com.savvagent.ottoflags.sdk

import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.test.runTest
import okhttp3.mockwebserver.Dispatcher
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import org.junit.After
import org.junit.Test
import org.junit.Assert.*

/**
 * An evaluate in flight when its flag is invalidated must not store its stale result (#137).
 * The mock answers `true` to the first request and `false` afterwards, and invalidates
 * [invalidateOnFirstRequest] while answering the first.
 */
class OttoFlagsClientInFlightInvalidationTest {

    private lateinit var mockWebServer: MockWebServer
    private lateinit var client: OttoFlagsClient
    private val requests = AtomicInteger()

    private fun start(invalidateOnFirstRequest: String) {
        mockWebServer = MockWebServer()
        mockWebServer.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val n = requests.incrementAndGet()
                if (n == 1) {
                    // The kill switch flips while this (stale) request is in flight.
                    client.clearConfigOverride(invalidateOnFirstRequest)
                }
                return MockResponse()
                    .setResponseCode(200)
                    .setBody("""{"enabled": ${n == 1}, "key": "checkout"}""")
                    .addHeader("Content-Type", "application/json")
            }
        }
        mockWebServer.start()
        client = OttoFlagsClient(
            OttoFlagsConfig(
                apiUrl = mockWebServer.url("/").toString().removeSuffix("/"),
                sdkKey = "test-key",
                environment = "test",
                enableWebSocket = false,
                pollingInterval = 0
            )
        )
    }

    @After
    fun tearDown() {
        client.close()
        mockWebServer.shutdown()
    }

    @Test
    fun testInvalidationDuringTheRequestDropsTheCacheWrite() = runTest {
        start("checkout")
        val user = UserContext(userId = "a")

        val first = client.evaluate("checkout", user).getOrThrow()
        val second = client.evaluate("checkout", user).getOrThrow()

        assertTrue(first.value)
        assertEquals("evaluated", second.reason)
        assertFalse(second.value)
        assertEquals(2, mockWebServer.requestCount)
        assertEquals(false, client.flagUpdates.value["checkout"])
    }

    @Test
    fun testInvalidatingAnotherFlagKeepsTheCacheWrite() = runTest {
        start("other")
        val user = UserContext(userId = "a")

        client.evaluate("checkout", user).getOrThrow()
        val second = client.evaluate("checkout", user).getOrThrow()

        assertEquals("cached", second.reason)
        assertEquals(1, mockWebServer.requestCount)
    }
}
