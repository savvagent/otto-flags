import XCTest
@testable import OttoFlagsSDK

/// An evaluate that is in flight when its flag is invalidated must not store its stale result
/// (savvagent/otto-flags#137).
final class InFlightInvalidationTests: XCTestCase {
    private var client: OttoFlagsClient!
    private let lock = NSLock()
    private var requestCount = 0
    private var invalidateOnFirstRequest: String?

    override func setUp() {
        super.setUp()
        requestCount = 0
        invalidateOnFirstRequest = nil
        MockURLProtocol.error = nil
        MockURLProtocol.requestHandler = { [weak self] request in
            guard let self else { throw URLError(.cancelled) }
            let (n, flagToInvalidate) = self.next()
            if n == 1, let flagToInvalidate {
                // The kill switch flips while this (stale) request is in flight.
                self.client.clearConfigOverride(flagKey: flagToInvalidate)
            }
            let response = HTTPURLResponse(
                url: request.url!,
                statusCode: 200,
                httpVersion: nil,
                headerFields: ["Content-Type": "application/json"]
            )!
            let body = """
            {"key": "checkout", "enabled": \(n == 1), "variation": null, "configuration": null}
            """.data(using: .utf8)!
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
        MockURLProtocol.error = nil
        super.tearDown()
    }

    private func next() -> (Int, String?) {
        lock.lock()
        defer { lock.unlock() }
        requestCount += 1
        return (requestCount, invalidateOnFirstRequest)
    }

    private func requests() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return requestCount
    }

    func testInvalidationDuringTheRequestDropsTheCacheWrite() async throws {
        invalidateOnFirstRequest = "checkout"
        let user = UserContext(userId: "a")

        let first = try await client.evaluate(flagKey: "checkout", context: user)
        let second = try await client.evaluate(flagKey: "checkout", context: user)

        XCTAssertTrue(first.value)
        XCTAssertEqual(second.reason, "evaluated")
        XCTAssertFalse(second.value)
        XCTAssertEqual(requests(), 2)
    }

    func testInvalidatingAnotherFlagKeepsTheCacheWrite() async throws {
        invalidateOnFirstRequest = "other"
        let user = UserContext(userId: "a")

        _ = try await client.evaluate(flagKey: "checkout", context: user)
        let second = try await client.evaluate(flagKey: "checkout", context: user)

        XCTAssertEqual(second.reason, "cached")
        XCTAssertEqual(requests(), 1)
    }
}
