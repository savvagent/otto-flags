import XCTest
@testable import OttoFlagsSDK

/// The evaluation cache is keyed by flag and by the request's context, so one user's
/// result is never served to another (savvagent/otto-flags#127).
final class ContextCacheTests: XCTestCase {
    private var client: OttoFlagsClient!
    private let lock = NSLock()
    private var requestCount = 0

    override func setUp() {
        super.setUp()
        requestCount = 0
        MockURLProtocol.error = nil
        MockURLProtocol.requestHandler = { [weak self] request in
            guard let self else { throw URLError(.cancelled) }
            let body = self.answer(request)
            let response = HTTPURLResponse(
                url: request.url!,
                statusCode: 200,
                httpVersion: nil,
                headerFields: ["Content-Type": "application/json"]
            )!
            return (response, body)
        }

        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MockURLProtocol.self]
        let session = URLSession(configuration: configuration)

        let config = OttoFlagsConfig(
            apiUrl: "https://otto-flags.example.com",
            sdkKey: "sdk_test",
            environment: "staging",
            pollingInterval: 3600,
            enableWebSocket: false
        )
        client = OttoFlagsClient(config: config, session: session)
    }

    override func tearDown() {
        client?.close()
        client = nil
        MockURLProtocol.requestHandler = nil
        MockURLProtocol.error = nil
        super.tearDown()
    }

    /// Counts the request and answers by the body's `context`: user `a` and plan `pro`
    /// get true, anyone else false.
    private func answer(_ request: URLRequest) -> Data {
        lock.lock()
        defer { lock.unlock() }
        guard request.url?.path != "/v1/flags" else { return Data() }
        requestCount += 1
        let data = request.httpBody ?? Self.read(request.httpBodyStream)
        let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        let context = json?["context"] as? [String: Any]
        let attributes = context?["attributes"] as? [String: Any]
        let enabled = (context?["user_id"] as? String) == "a" || (attributes?["plan"] as? String) == "pro"
        return """
        {"key": "checkout", "enabled": \(enabled), "variation": null, "configuration": null}
        """.data(using: .utf8)!
    }

    private static func read(_ stream: InputStream?) -> Data {
        guard let stream else { return Data() }
        stream.open()
        defer { stream.close() }
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            if count <= 0 { break }
            data.append(buffer, count: count)
        }
        return data
    }

    private func requests() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return requestCount
    }

    func testEachUserGetsTheirOwnCachedResult() async throws {
        let a = UserContext(userId: "a")
        let b = UserContext(userId: "b")

        let first = try await client.evaluate(flagKey: "checkout", context: a)
        let second = try await client.evaluate(flagKey: "checkout", context: b)
        let third = try await client.evaluate(flagKey: "checkout", context: a)
        let fourth = try await client.evaluate(flagKey: "checkout", context: b)

        XCTAssertEqual([first.value, second.value, third.value, fourth.value], [true, false, true, false])
        XCTAssertEqual(
            [first.reason, second.reason, third.reason, fourth.reason],
            ["evaluated", "evaluated", "cached", "cached"]
        )
        XCTAssertEqual(requests(), 2)
    }

    func testContextsDifferingOnlyInAnAttributeAreCachedSeparately() async throws {
        let pro = UserContext(userId: "u", attributes: ["plan": "pro"])
        let free = UserContext(userId: "u", attributes: ["plan": "free"])

        let proResult = try await client.evaluate(flagKey: "checkout", context: pro)
        let freeResult = try await client.evaluate(flagKey: "checkout", context: free)

        XCTAssertTrue(proResult.value)
        XCTAssertFalse(freeResult.value)
        XCTAssertEqual(proResult.reason, "evaluated")
        XCTAssertEqual(freeResult.reason, "evaluated")
        XCTAssertEqual(requests(), 2)
    }

    func testSameAttributesInAnyOrderShareAnEntry() async throws {
        let one = UserContext(userId: "u", attributes: ["plan": "pro", "region": "eu"])
        let two = UserContext(userId: "u", attributes: ["region": "eu", "plan": "pro"])

        _ = try await client.evaluate(flagKey: "checkout", context: one)
        let second = try await client.evaluate(flagKey: "checkout", context: two)

        XCTAssertEqual(second.reason, "cached")
        XCTAssertEqual(requests(), 1)
    }

    func testOverrideChangesDropEveryContextOfTheFlag() async throws {
        let a = UserContext(userId: "a")
        let b = UserContext(userId: "b")
        _ = try await client.evaluate(flagKey: "checkout", context: a)
        _ = try await client.evaluate(flagKey: "checkout", context: b)
        XCTAssertEqual(requests(), 2)

        client.setConfigOverride(flagKey: "checkout", config: ["x": 1])
        let afterSet = try await client.evaluate(flagKey: "checkout", context: a)
        _ = try await client.evaluate(flagKey: "checkout", context: b)
        XCTAssertEqual(afterSet.reason, "evaluated")
        XCTAssertEqual(requests(), 4)

        client.clearConfigOverride(flagKey: "checkout")
        let afterClear = try await client.evaluate(flagKey: "checkout", context: a)
        _ = try await client.evaluate(flagKey: "checkout", context: b)
        XCTAssertEqual(afterClear.reason, "evaluated")
        XCTAssertEqual(requests(), 6)
    }

    /// Overrides are changed, read and applied by evaluations from many tasks at once. Before
    /// savvagent/otto-flags#138 the dictionaries were unsynchronized, so this could crash.
    func testConcurrentOverrideChangesAndEvaluations() async {
        let flag = "concurrent"
        let context = UserContext(userId: "u")
        let client = self.client!

        await withTaskGroup(of: Void.self) { group in
            for i in 0..<60 {
                group.addTask {
                    switch i % 6 {
                    case 0: client.setConfigOverride(flagKey: flag, config: ["v": i])
                    case 1: client.setVariationOverride(flagKey: flag, variation: "var\(i)")
                    case 2: client.clearConfigOverride(flagKey: flag)
                    case 3:
                        _ = client.hasConfigOverride(flagKey: flag)
                        _ = client.getVariationOverrides()
                        _ = client.getConfigOverrides()
                    case 4: client.clearVariationOverride(flagKey: flag)
                    default: _ = try? await client.evaluate(flagKey: flag, context: context)
                    }
                }
                group.addTask { _ = try? await client.evaluate(flagKey: flag, context: context) }
            }
        }

        client.clearAllOverrides()
        XCTAssertFalse(client.hasConfigOverride(flagKey: flag))
        XCTAssertFalse(client.hasVariationOverride(flagKey: flag))
    }
}
