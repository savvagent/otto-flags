package com.savvagent.ottoflags.sdk

import kotlinx.coroutines.test.runTest
import okhttp3.mockwebserver.Dispatcher
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.junit.Assert.*

/**
 * The flag-list poll follows docs/SDK-DEVELOPER-GUIDE.md (#125):
 * `GET /api/sdk/flags?environment=<env>`, flags in `{"flags": [{"key", "enabled", ...}], "count", ...}`.
 */
class OttoFlagsClientFlagListTest {

    private lateinit var mockWebServer: MockWebServer
    private lateinit var client: OttoFlagsClient
    private val listRequests = mutableListOf<RecordedRequest>()

    @Volatile private var listStatus = 200
    @Volatile private var listBody = SERVER_LIST

    @Before
    fun setUp() {
        mockWebServer = MockWebServer()
        mockWebServer.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val path = request.requestUrl?.encodedPath
                return if (path == "/api/sdk/flags") {
                    synchronized(listRequests) { listRequests.add(request) }
                    MockResponse().setResponseCode(listStatus).setBody(listBody)
                } else {
                    MockResponse()
                        .setResponseCode(200)
                        .setBody("""{"enabled": true, "variation": null, "configuration": null}""")
                }
            }
        }
        mockWebServer.start()

        client = OttoFlagsClient(
            OttoFlagsConfig(
                apiUrl = mockWebServer.url("/").toString().removeSuffix("/"),
                sdkKey = "test-key",
                environment = "staging",
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

    private suspend fun reason(key: String) =
        client.evaluate(key, UserContext(userId = "u1")).getOrThrow().reason

    @Test
    fun testPollCallsDocumentedEndpoint() = runTest {
        client.refreshFlags()

        assertEquals(1, listRequests.size)
        val request = listRequests[0]
        assertEquals("GET", request.method)
        assertEquals("/api/sdk/flags", request.requestUrl?.encodedPath)
        assertEquals("staging", request.requestUrl?.queryParameter("environment"))
        assertEquals("Bearer test-key", request.getHeader("Authorization"))
    }

    @Test
    fun testPollDropsCacheOfListedFlagsOnly() = runTest {
        reason("dark-mode")
        reason("other-flag")
        assertEquals("cached", reason("dark-mode"))

        client.refreshFlags()

        assertEquals("evaluated", reason("dark-mode"))
        assertEquals("cached", reason("other-flag"))
    }

    @Test
    fun testPollIgnoresErrorStatusAndMalformedBody() = runTest {
        reason("dark-mode")

        listStatus = 500
        client.refreshFlags()
        assertEquals("cached", reason("dark-mode"))

        listStatus = 200
        listBody = "not json"
        client.refreshFlags()
        assertEquals("cached", reason("dark-mode"))
    }

    companion object {
        /** The list response as the server sends it. */
        private const val SERVER_LIST = """
            {"flags": [
              {"key": "dark-mode", "enabled": true, "scope": "application",
               "environments": {"staging": {"enabled": true, "rollout_percentage": 100}},
               "variations": null, "configuration": {"theme": "dark-blue"}, "version": 5}
            ],
             "count": 1, "organization_id": "org-uuid", "application_id": "app-uuid"}
        """
    }
}
