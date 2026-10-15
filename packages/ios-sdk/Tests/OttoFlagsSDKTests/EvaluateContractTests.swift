import XCTest
@testable import OttoFlagsSDK

/// The evaluate call follows docs/SDK-DEVELOPER-GUIDE.md (savvagent/otto-flags#112):
/// `POST /api/flags/{key}/evaluate` with `{"context": {...}}`, the result in `enabled`.
final class EvaluateContractTests: XCTestCase {
    private var client: OttoFlagsClient!
    private let lock = NSLock()
    private var requests: [URLRequest] = []
    private var bodies: [Data] = []
    private var responseBody = Data()

    override func setUp() {
        super.setUp()
        requests = []
        bodies = []
        responseBody = Self.serverBody(variation: nil)
        MockURLProtocol.error = nil
        MockURLProtocol.requestHandler = { [weak self] request in
            guard let self else { throw URLError(.cancelled) }
            let body = self.record(request)
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

    /// The evaluate response exactly as the server sends it.
    private static func serverBody(variation: String?) -> Data {
        let variationJSON = variation.map { "\"\($0)\"" } ?? "null"
        return """
        {"key": "checkout", "enabled": true, "scope": "application", "variation": \(variationJSON),
         "configuration": null, "timestamp": 1700000000, "version": 3, "context": {}}
        """.data(using: .utf8)!
    }

    /// Records a flag request and its body, and returns the response body to send.
    /// URLProtocol sees the body as a stream, not `httpBody`.
    private func record(_ request: URLRequest) -> Data {
        lock.lock()
        defer { lock.unlock() }
        guard request.url?.path != "/v1/flags" else { return Data() }
        requests.append(request)
        bodies.append(request.httpBody ?? Self.read(request.httpBodyStream))
        return responseBody
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

    private func setResponse(variation: String?) {
        lock.lock()
        responseBody = Self.serverBody(variation: variation)
        lock.unlock()
    }

    private func recorded() -> ([URLRequest], [Data]) {
        lock.lock()
        defer { lock.unlock() }
        return (requests, bodies)
    }

    func testEvaluatePostsTheDocumentedPathAndBody() async throws {
        let context = UserContext(userId: "user-123", attributes: ["plan": "pro"])
        let result = try await client.evaluate(flagKey: "checkout", context: context)

        XCTAssertTrue(result.value)
        XCTAssertNil(result.variation)
        XCTAssertNil(result.configuration)
        let (requests, bodies) = recorded()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.httpMethod, "POST")
        XCTAssertEqual(requests.first?.url?.path, "/api/flags/checkout/evaluate")

        let json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: try XCTUnwrap(bodies.first)) as? [String: Any]
        )
        let sent = try XCTUnwrap(json["context"] as? [String: Any])
        XCTAssertEqual(sent["user_id"] as? String, "user-123")
        XCTAssertEqual(sent["environment"] as? String, "staging")
        XCTAssertEqual((sent["attributes"] as? [String: Any])?["plan"] as? String, "pro")
    }

    func testGetVariationReturnsTheVariationName() async {
        setResponse(variation: "treatment-a")
        let value = await client.getVariation(
            flagKey: "checkout",
            context: UserContext(userId: "u"),
            defaultValue: "fallback"
        )
        XCTAssertEqual(value, "treatment-a")
        XCTAssertEqual(recorded().0.first?.url?.path, "/api/flags/checkout/evaluate")
    }

    func testGetVariationReturnsTheDefaultWithoutAVariation() async {
        let value = await client.getVariation(
            flagKey: "checkout",
            context: UserContext(userId: "u"),
            defaultValue: "fallback"
        )
        XCTAssertEqual(value, "fallback")
    }

    func testGetVariationWithABoolDefaultReturnsEnabled() async {
        let value = await client.getVariation(
            flagKey: "checkout",
            context: UserContext(userId: "u"),
            defaultValue: false
        )
        XCTAssertTrue(value)
    }
}
