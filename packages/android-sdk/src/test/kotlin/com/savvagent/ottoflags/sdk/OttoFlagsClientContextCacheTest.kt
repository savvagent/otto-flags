package com.savvagent.ottoflags.sdk

import kotlinx.coroutines.test.runTest
import okhttp3.mockwebserver.Dispatcher
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import org.json.JSONObject
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.junit.Assert.*

/**
 * The evaluation cache is per flag and per context (#127): one user's result is never
 * served to another. The mock answers by the request body's user_id and plan attribute.
 */
class OttoFlagsClientContextCacheTest {

    private lateinit var mockWebServer: MockWebServer
    private lateinit var client: OttoFlagsClient

    @Before
    fun setUp() {
        mockWebServer = MockWebServer()
        mockWebServer.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val context = JSONObject(request.body.readUtf8()).getJSONObject("context")
                val attributes = context.optJSONObject("attributes")
                val enabled = context.optString("user_id") == "a" ||
                    attributes?.optString("plan") == "pro"
                return MockResponse()
                    .setResponseCode(200)
                    .setBody("""{"enabled": $enabled, "key": "checkout"}""")
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
    fun testDifferentUsersGetTheirOwnAnswers() = runTest {
        val a = UserContext(userId = "a")
        val b = UserContext(userId = "b")

        val results = listOf(a, b, a, b).map { client.evaluate("checkout", it).getOrThrow() }

        assertEquals(listOf(true, false, true, false), results.map { it.value })
        assertEquals(
            listOf("evaluated", "evaluated", "cached", "cached"),
            results.map { it.reason }
        )
        assertEquals(2, mockWebServer.requestCount)
    }

    @Test
    fun testAttributesArePartOfTheContext() = runTest {
        val pro = UserContext(userId = "b", attributes = mapOf("plan" to "pro"))
        val free = UserContext(userId = "b", attributes = mapOf("plan" to "free"))

        assertTrue(client.evaluate("checkout", pro).getOrThrow().value)
        assertFalse(client.evaluate("checkout", free).getOrThrow().value)
        assertEquals(2, mockWebServer.requestCount)
    }

    @Test
    fun testAttributeInsertionOrderDoesNotMatter() = runTest {
        val first = UserContext(
            userId = "a",
            attributes = linkedMapOf<String, Any>("plan" to "pro", "region" to "eu")
        )
        val second = UserContext(
            userId = "a",
            attributes = linkedMapOf<String, Any>("region" to "eu", "plan" to "pro")
        )

        assertEquals("evaluated", client.evaluate("checkout", first).getOrThrow().reason)
        assertEquals("cached", client.evaluate("checkout", second).getOrThrow().reason)
        assertEquals(1, mockWebServer.requestCount)
    }

    @Test
    fun testOverrideSettersInvalidateEveryContext() = runTest {
        val a = UserContext(userId = "a")
        val b = UserContext(userId = "b")
        client.evaluate("checkout", a)
        client.evaluate("checkout", b)
        assertEquals(2, mockWebServer.requestCount)

        client.setVariationOverride("checkout", "blue")
        assertEquals("evaluated", client.evaluate("checkout", a).getOrThrow().reason)
        assertEquals("evaluated", client.evaluate("checkout", b).getOrThrow().reason)
        assertEquals(4, mockWebServer.requestCount)

        client.clearVariationOverride("checkout")
        assertEquals("evaluated", client.evaluate("checkout", a).getOrThrow().reason)
        assertEquals("evaluated", client.evaluate("checkout", b).getOrThrow().reason)
        assertEquals(6, mockWebServer.requestCount)
    }

    @Test
    fun testFlagUpdatesHoldTheLatestEvaluatedValue() = runTest {
        client.evaluate("checkout", UserContext(userId = "a"))
        assertEquals(true, client.flagUpdates.value["checkout"])

        client.evaluate("checkout", UserContext(userId = "b"))
        assertEquals(false, client.flagUpdates.value["checkout"])
    }

    @Test
    fun testContextKeyIsIndependentOfKeyOrder() {
        fun body(vararg attrs: Pair<String, Any>) = JSONObject().apply {
            put("context", JSONObject().apply {
                put("user_id", "a")
                put("attributes", JSONObject(linkedMapOf<String, Any>(*attrs)))
            })
        }

        assertEquals(
            client.contextKey(body("x" to 1, "y" to "two")),
            client.contextKey(body("y" to "two", "x" to 1))
        )
        assertNotEquals(
            client.contextKey(body("x" to 1)),
            client.contextKey(body("x" to 2))
        )
    }
}
