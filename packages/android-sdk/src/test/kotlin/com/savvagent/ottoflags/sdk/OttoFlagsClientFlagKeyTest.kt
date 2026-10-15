package com.savvagent.ottoflags.sdk

import kotlinx.coroutines.test.runTest
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.junit.Assert.*
import java.util.concurrent.TimeUnit

/**
 * Flag keys are interpolated into the request URL, so a key outside the server's rule
 * must be rejected before any request (and the SDK key) leaves the device (#107).
 *
 * Logging is off on purpose: android.util.Log is a stub in local unit tests.
 */
class OttoFlagsClientFlagKeyTest {

    private lateinit var mockWebServer: MockWebServer
    private lateinit var client: OttoFlagsClient

    private val invalidKeys = listOf(
        "a/b", "a?b", "a#b", ".", "..", "", "bad key", "a".repeat(129)
    )

    @Before
    fun setUp() {
        mockWebServer = MockWebServer()
        mockWebServer.start()

        client = OttoFlagsClient(
            OttoFlagsConfig(
                apiUrl = mockWebServer.url("/").toString().removeSuffix("/"),
                sdkKey = "test-key",
                environment = "test",
                enableWebSocket = false,
                pollingInterval = 0,
                enableLogging = false
            )
        )
    }

    @After
    fun tearDown() {
        client.close()
        mockWebServer.shutdown()
    }

    @Test
    fun evaluateRejectsInvalidKeysWithoutARequest() = runTest {
        val context = UserContext(userId = "user-123")
        for (key in invalidKeys) {
            val result = client.evaluate(key, context)
            assertTrue("expected failure for key of length ${key.length}", result.isFailure)
            assertTrue(result.exceptionOrNull() is OttoFlagsException)
        }
        assertEquals(0, mockWebServer.requestCount)
    }

    @Test
    fun getVariationReturnsDefaultForInvalidKeysWithoutARequest() = runTest {
        val context = UserContext(userId = "user-123")
        for (key in invalidKeys) {
            assertEquals("fallback", client.getVariation(key, context, "fallback"))
        }
        assertEquals(0, mockWebServer.requestCount)
    }

    @Test
    fun invalidKeyMessageEscapesAndTruncatesTheKey() = runTest {
        val key = "a\"b\\c\u0000é" + "x".repeat(200)
        val failure = client.evaluate(key, UserContext(userId = "u")).exceptionOrNull()
        assertTrue(failure is OttoFlagsException)
        val message = failure!!.message!!
        assertTrue(message, message.startsWith("Invalid flag key \"a\\u0022b\\u005cc\\u0000\\u00e9"))
        assertTrue(message, message.endsWith("(${key.length} characters); not evaluated"))
        assertFalse(message, message.contains('\u0000'))
    }

    @Test
    fun validKeyKeepsTheExistingEvaluatePath() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("""{"enabled":true}""")
                .addHeader("Content-Type", "application/json")
        )

        client.evaluate("new-checkout.v2_b", UserContext(userId = "user-123"))

        val request = mockWebServer.takeRequest(5, TimeUnit.SECONDS)
        assertNotNull("no request reached the server", request)
        assertEquals("/v1/flags/new-checkout.v2_b/evaluate", request!!.path)
    }
}
