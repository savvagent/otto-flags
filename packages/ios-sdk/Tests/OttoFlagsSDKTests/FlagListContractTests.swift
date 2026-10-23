import XCTest
@testable import OttoFlagsSDK

/// The flag-list poll follows docs/SDK-DEVELOPER-GUIDE.md (savvagent/otto-flags#125):
/// `GET /api/sdk/flags?environment=<env>`, flags in `{"flags": [{"key", "enabled", ...}], "count", ...}`.
final class FlagListContractTests: XCTestCase {
    private var client: OttoFlagsClient!
    private let lock = NSLock()
    private var listRequests: [URLRequest] = []
    private var evaluateCount = 0
    private var listStatus = 200
    private var listBody = Data()

    /// The list response as the server sends it.
    private static let serverList = """
    {"flags": [
        {"key": "dark-mode", "enabled": true, "scope": "application",
         "environments": {"staging": {"enabled": true, "rollout_percentage": 100}},
         "variations": null, "configuration": {"theme": "dark-blue"}, "version": 5}
      ],
     "count": 1, "organization_id": "org-uuid", "application_id": "app-uuid"}
    """.data(using: .utf8)!

    override func setUp() {
        super.setUp()
        listRequests = []
        evaluateCount = 0
        listStatus = 200
        listBody = Self.serverList
        MockURLProtocol.error = nil
        MockURLProtocol.requestHandler = { [weak self] request in
            guard let self else { throw URLError(.cancelled) }
            self.lock.lock()
            defer { self.lock.unlock() }
            let isList = request.url?.path == "/api/sdk/flags"
            if isList { self.listRequests.append(request) } else { self.evaluateCount += 1 }
            let response = HTTPURLResponse(
                url: request.url!,
                statusCode: isList ? self.listStatus : 200,
                httpVersion: nil,
                headerFields: ["Content-Type": "application/json"]
            )!
            let body = isList
                ? self.listBody
                : "{\"enabled\": true, \"variation\": null, \"configuration\": null}".data(using: .utf8)!
            return (response, body)
        }

        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MockURLProtocol.self]
        let config = OttoFlagsConfig(
            apiUrl: "https://otto-flags.example.com",
            sdkKey: "sdk_test",
            environment: "staging",
            pollingInterval: 3600,
            enableWebSocket: false
        )
        client = OttoFlagsClient(config: config, session: URLSession(configuration: configuration))
    }

    override func tearDown() {
        client?.close()
        client = nil
        MockURLProtocol.requestHandler = nil
        super.tearDown()
    }

    private func reason(_ key: String) async throws -> String {
        try await client.evaluate(flagKey: key, context: UserContext(userId: "u1")).reason
    }

    func testPollCallsDocumentedEndpoint() async {
        await client.refreshFlags()

        XCTAssertEqual(listRequests.count, 1)
        let request = listRequests[0]
        XCTAssertEqual(request.httpMethod, "GET")
        XCTAssertEqual(request.url?.path, "/api/sdk/flags")
        XCTAssertEqual(request.url?.query, "environment=staging")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer sdk_test")
    }

    func testPollDropsCacheOfListedFlagsOnly() async throws {
        _ = try await reason("dark-mode")
        _ = try await reason("other-flag")
        let cachedDark = try await reason("dark-mode")
        XCTAssertEqual(cachedDark, "cached")

        await client.refreshFlags()

        let darkAfter = try await reason("dark-mode")
        let otherAfter = try await reason("other-flag")
        XCTAssertEqual(darkAfter, "evaluated")
        XCTAssertEqual(otherAfter, "cached")
    }

    func testPollIgnoresErrorStatusAndMalformedBody() async throws {
        _ = try await reason("dark-mode")

        lock.lock(); listStatus = 500; lock.unlock()
        await client.refreshFlags()
        let afterError = try await reason("dark-mode")
        XCTAssertEqual(afterError, "cached")

        lock.lock(); listStatus = 200; listBody = "not json".data(using: .utf8)!; lock.unlock()
        await client.refreshFlags()
        let afterGarbage = try await reason("dark-mode")
        XCTAssertEqual(afterGarbage, "cached")
    }
}
