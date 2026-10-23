package com.savvagent.ottoflags.sdk

import kotlinx.coroutines.test.runTest
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.junit.Assert.*

class OttoFlagsClientNetworkTest {

    private lateinit var mockWebServer: MockWebServer
    private lateinit var client: OttoFlagsClient

    @Before
    fun setUp() {
        mockWebServer = MockWebServer()
        mockWebServer.start()

        val config = OttoFlagsConfig(
            apiUrl = mockWebServer.url("/").toString().removeSuffix("/"),
            sdkKey = "test-key",
            environment = "test",
            enableWebSocket = false,
            pollingInterval = 0,
            enableLogging = true
        )

        client = OttoFlagsClient(config)
    }

    @After
    fun tearDown() {
        client.close()
        mockWebServer.shutdown()
    }

    @Test
    fun testIsEnabledReturnsTrue() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("""{"enabled": true, "key": "test-flag"}""")
                .addHeader("Content-Type", "application/json")
        )

        val context = UserContext(userId = "user-123")
        val result = client.isEnabled("test-flag", context)

        assertTrue(result.isSuccess)
        assertTrue(result.getOrDefault(false))

        val request = mockWebServer.takeRequest()
        assertEquals("/api/flags/test-flag/evaluate", request.path)
        assertEquals("POST", request.method)
        assertEquals("Bearer test-key", request.getHeader("Authorization"))
    }

    @Test
    fun testIsEnabledReturnsFalse() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("""{"enabled": false, "key": "test-flag"}""")
                .addHeader("Content-Type", "application/json")
        )

        val context = UserContext(userId = "user-123")
        val result = client.isEnabled("test-flag", context)

        assertTrue(result.isSuccess)
        assertFalse(result.getOrDefault(true))
    }

    @Test
    fun testIsEnabledHandles404() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(404)
                .setBody("""{"error": "Flag not found"}""")
        )

        val context = UserContext(userId = "user-123")
        val result = client.isEnabled("missing-flag", context)

        assertTrue(result.isFailure)
    }

    @Test
    fun testIsEnabledHandles500() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(500)
                .setBody("Internal server error")
        )

        val context = UserContext(userId = "user-123")
        val result = client.isEnabled("test-flag", context)

        assertTrue(result.isFailure)
    }

    @Test
    fun testIsEnabledHandlesInvalidJSON() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("invalid json")
        )

        val context = UserContext(userId = "user-123")
        val result = client.isEnabled("test-flag", context)

        assertTrue(result.isFailure)
    }

    @Test
    fun testIsEnabledSendsUserContext() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("""{"enabled": true}""")
        )

        val context = UserContext(
            userId = "user-456",
            attributes = mapOf(
                "email" to "test@example.com",
                "plan" to "pro"
            )
        )

        client.isEnabled("test-flag", context)

        val request = mockWebServer.takeRequest()
        val body = org.json.JSONObject(request.body.readUtf8())

        // {"context": {...}} with snake_case fields (docs/SDK-DEVELOPER-GUIDE.md, #112).
        val sent = body.getJSONObject("context")
        assertEquals("user-456", sent.getString("user_id"))
        assertEquals("test", sent.getString("environment"))
        assertEquals("test@example.com", sent.getJSONObject("attributes").getString("email"))
    }

    @Test
    fun testGetVariationReturnsValue() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("""{"key": "theme", "enabled": true, "variation": "dark-mode", "configuration": null}""")
        )

        val context = UserContext(userId = "user-123")
        val value = client.getVariation("theme", context, "light")

        assertEquals("dark-mode", value)
        // The server has no variation endpoint; getVariation evaluates the flag (#112).
        assertEquals("/api/flags/theme/evaluate", mockWebServer.takeRequest().path)
    }

    @Test
    fun testEvaluateAcceptsNullVariationAndConfiguration() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody(
                    """{"key": "test-flag", "enabled": true, "scope": "application", "variation": null, "configuration": null, "timestamp": 1700000000, "version": 3, "context": {}}"""
                )
        )

        val result = client.evaluate("test-flag", UserContext(userId = "user-123"))

        assertTrue(result.isSuccess)
        val evaluation = result.getOrThrow()
        assertTrue(evaluation.value)
        assertNull(evaluation.variation)
        assertNull(evaluation.configuration)
    }

    @Test
    fun testEvaluateWithoutBooleanEnabledFails() = runTest {
        mockWebServer.enqueue(MockResponse().setResponseCode(200).setBody("""{"value": true}"""))
        mockWebServer.enqueue(MockResponse().setResponseCode(200).setBody("""{"enabled": "true"}"""))

        val context = UserContext(userId = "user-123")
        assertTrue(client.evaluate("test-flag", context).isFailure)
        assertTrue(client.evaluate("test-flag", context).isFailure)
        // Not cached: the second lookup asks the server again.
        assertEquals(2, mockWebServer.requestCount)
    }

    @Test
    fun testGetVariationReturnsDefaultOnError() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(500)
        )

        val context = UserContext(userId = "user-123")
        val value = client.getVariation("theme", context, "light")

        assertEquals("light", value)
    }

    @Test
    fun testGetVariationWithNumericValue() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("""{"key": "max-items", "enabled": true, "variation": "big", "configuration": null}""")
        )

        val context = UserContext(userId = "user-123")
        val value = client.getVariation("max-items", context, 10)

        // The evaluate response has no numeric value; a non-String, non-Boolean default
        // comes back unchanged.
        assertEquals(10, value)
    }

    // track is a documented no-op (#125): the server has no custom-event endpoint.
    @Test
    fun testTrackSendsNoRequest() = runTest {
        client.track("button_click", UserContext(userId = "user-123"), mapOf("button" to "checkout"))

        assertEquals(0, mockWebServer.requestCount)
    }

    @Test
    fun testTrackEventDoesNotThrowOnFailure() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(500)
        )

        val context = UserContext(userId = "user-123")

        // Should not throw
        client.track("test_event", context)
    }

    @Test
    fun testMultipleSequentialRequests() = runTest {
        // Queue multiple responses
        repeat(3) {
            mockWebServer.enqueue(
                MockResponse()
                    .setResponseCode(200)
                    .setBody("""{"enabled": true}""")
            )
        }

        val context = UserContext(userId = "user-123")

        val result1 = client.isEnabled("flag1", context)
        val result2 = client.isEnabled("flag2", context)
        val result3 = client.isEnabled("flag3", context)

        assertTrue(result1.isSuccess)
        assertTrue(result2.isSuccess)
        assertTrue(result3.isSuccess)

        assertEquals(3, mockWebServer.requestCount)
    }

    @Test
    fun testRequestHeaders() = runTest {
        mockWebServer.enqueue(
            MockResponse()
                .setResponseCode(200)
                .setBody("""{"enabled": true}""")
        )

        val context = UserContext(userId = "user-123")
        client.isEnabled("test-flag", context)

        val request = mockWebServer.takeRequest()

        assertEquals("Bearer test-key", request.getHeader("Authorization"))
        assertEquals("application/json", request.getHeader("Content-Type"))
    }
}
