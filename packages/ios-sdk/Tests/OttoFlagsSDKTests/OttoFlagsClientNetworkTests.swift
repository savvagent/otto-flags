import XCTest
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
@testable import OttoFlagsSDK

/// Drives the client through its internal `init(config:session:)` with a URLProtocol stub, so
/// each test asserts on the request the SDK sent and on what it does with the response.
final class OttoFlagsClientNetworkTests: XCTestCase {
    private var client: OttoFlagsClient!
    private let lock = NSLock()
    private var requests: [URLRequest] = []
    private var bodies: [Data] = []

    override func setUp() {
        super.setUp()
        requests = []
        bodies = []
        MockURLProtocol.error = nil
        MockURLProtocol.requestHandler = nil

        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MockURLProtocol.self]
        let session = URLSession(configuration: configuration)

        let config = OttoFlagsConfig(
            apiUrl: "https://otto-flags.example.com",
            sdkKey: "test-key",
            environment: "test",
            pollingInterval: 0, // Disable polling for tests
            enableWebSocket: false
        )
        client = OttoFlagsClient(config: config, session: session)
    }

    override func tearDown() {
        MockURLProtocol.requestHandler = nil
        MockURLProtocol.error = nil
        client?.close()
        client = nil
        super.tearDown()
    }

    /// Answers every request with `status` and `body`, recording the request and its body
    /// (URLProtocol sees the body as a stream, not `httpBody`).
    private func respond(status: Int = 200, body: String = "{}") {
        MockURLProtocol.requestHandler = { [weak self] request in
            self?.record(request)
            let response = HTTPURLResponse(
                url: request.url!,
                statusCode: status,
                httpVersion: nil,
                headerFields: ["Content-Type": "application/json"]
            )!
            return (response, body.data(using: .utf8)!)
        }
    }

    private func record(_ request: URLRequest) {
        lock.lock()
        defer { lock.unlock() }
        requests.append(request)
        bodies.append(request.httpBody ?? Self.read(request.httpBodyStream))
    }

    private func recorded() -> (requests: [URLRequest], bodies: [Data]) {
        lock.lock()
        defer { lock.unlock() }
        return (requests, bodies)
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

    func testIsEnabledReturnsTrue() async throws {
        respond(body: #"{"key": "test-flag", "enabled": true}"#)

        let enabled = try await client.isEnabled(flagKey: "test-flag", context: UserContext(userId: "user-123"))

        XCTAssertTrue(enabled)
        let sent = recorded().requests
        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent.first?.httpMethod, "POST")
        XCTAssertEqual(sent.first?.url?.path, "/api/flags/test-flag/evaluate")
        XCTAssertEqual(sent.first?.value(forHTTPHeaderField: "Authorization"), "Bearer test-key")
    }

    func testIsEnabledReturnsFalse() async throws {
        respond(body: #"{"key": "test-flag", "enabled": false}"#)

        let enabled = try await client.isEnabled(flagKey: "test-flag", context: UserContext(userId: "user-123"))

        XCTAssertFalse(enabled)
    }

    func testIsEnabledThrowsRequestFailedOnHTTPError() async {
        respond(status: 404, body: "")

        do {
            _ = try await client.isEnabled(flagKey: "test-flag", context: UserContext(userId: "user-123"))
            XCTFail("expected OttoFlagsError.requestFailed")
        } catch OttoFlagsError.requestFailed {
            // expected
        } catch {
            XCTFail("expected OttoFlagsError.requestFailed, got \(error)")
        }
    }

    func testIsEnabledThrowsInvalidResponseWhenEnabledIsMissing() async {
        respond(body: #"{"key": "test-flag"}"#)

        do {
            _ = try await client.isEnabled(flagKey: "test-flag", context: UserContext(userId: "user-123"))
            XCTFail("expected OttoFlagsError.invalidResponse")
        } catch OttoFlagsError.invalidResponse {
            // expected
        } catch {
            XCTFail("expected OttoFlagsError.invalidResponse, got \(error)")
        }
    }

    func testIsEnabledPropagatesNetworkError() async {
        MockURLProtocol.error = URLError(.notConnectedToInternet)

        do {
            _ = try await client.isEnabled(flagKey: "test-flag", context: UserContext(userId: "user-123"))
            XCTFail("expected a network error")
        } catch let error as URLError {
            XCTAssertEqual(error.code, .notConnectedToInternet)
        } catch {
            XCTFail("expected a URLError, got \(error)")
        }
    }

    func testGetVariationReturnsStringValue() async {
        respond(body: #"{"key": "theme", "enabled": true, "variation": "dark-mode", "configuration": null}"#)

        let value = await client.getVariation(
            flagKey: "theme",
            context: UserContext(userId: "user-123"),
            defaultValue: "light"
        )

        XCTAssertEqual(value, "dark-mode")
    }

    func testGetVariationReturnsDefaultOnError() async {
        MockURLProtocol.error = URLError(.notConnectedToInternet)

        let value = await client.getVariation(
            flagKey: "theme",
            context: UserContext(userId: "user-123"),
            defaultValue: "light"
        )

        XCTAssertEqual(value, "light")
    }

    func testTrackEventSendsCorrectPayload() async throws {
        respond()

        await client.track(
            eventName: "button_click",
            context: UserContext(userId: "user-123"),
            properties: ["screen": "home"]
        )

        let (sent, bodies) = recorded()
        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent.first?.httpMethod, "POST")
        XCTAssertEqual(sent.first?.value(forHTTPHeaderField: "Authorization"), "Bearer test-key")
        let json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: try XCTUnwrap(bodies.first)) as? [String: Any]
        )
        XCTAssertEqual(json["event"] as? String, "button_click")
        XCTAssertEqual((json["properties"] as? [String: Any])?["screen"] as? String, "home")
        XCTAssertNotNil(json["timestamp"])
    }

    func testTrackDoesNotThrowOnNetworkError() async {
        MockURLProtocol.error = URLError(.notConnectedToInternet)

        await client.track(eventName: "button_click", context: UserContext(userId: "user-123"))

        XCTAssertTrue(recorded().requests.isEmpty)
    }

    func testUserContextSerializesToJSON() {
        let context = UserContext(
            userId: "user-123",
            attributes: [
                "email": "test@example.com",
                "plan": "pro",
                "age": 25,
                "premium": true
            ]
        )

        let json = context.toJSON()

        XCTAssertEqual(json["userId"] as? String, "user-123")

        if let attrs = json["attributes"] as? [String: Any] {
            XCTAssertEqual(attrs["email"] as? String, "test@example.com")
            XCTAssertEqual(attrs["plan"] as? String, "pro")
            XCTAssertEqual(attrs["age"] as? Int, 25)
            XCTAssertEqual(attrs["premium"] as? Bool, true)
        } else {
            XCTFail("Attributes not found")
        }
    }

    func testCachingWorks() async throws {
        respond(body: #"{"key": "test-flag", "enabled": true}"#)
        let context = UserContext(userId: "user-123")

        let first = try await client.evaluate(flagKey: "test-flag", context: context)
        let second = try await client.evaluate(flagKey: "test-flag", context: context)

        XCTAssertEqual(first.reason, "evaluated")
        XCTAssertEqual(second.reason, "cached")
        XCTAssertTrue(second.value)
        XCTAssertEqual(recorded().requests.count, 1, "the second evaluation must come from the cache")
    }

    func testConfigurationDefaults() {
        let config = OttoFlagsConfig(sdkKey: "test-key")

        XCTAssertEqual(config.apiUrl, "https://otto-flags.savvagent.com")
        XCTAssertEqual(config.environment, "production")
        XCTAssertEqual(config.pollingInterval, 60)
        XCTAssertTrue(config.enableWebSocket)
        XCTAssertEqual(config.timeout, 30)
    }

    func testConfigurationCustomValues() {
        let config = OttoFlagsConfig(
            apiUrl: "https://custom.api.com",
            sdkKey: "custom-key",
            environment: "staging",
            pollingInterval: 120,
            enableWebSocket: false,
            timeout: 45
        )

        XCTAssertEqual(config.apiUrl, "https://custom.api.com")
        XCTAssertEqual(config.sdkKey, "custom-key")
        XCTAssertEqual(config.environment, "staging")
        XCTAssertEqual(config.pollingInterval, 120)
        XCTAssertFalse(config.enableWebSocket)
        XCTAssertEqual(config.timeout, 45)
    }
}
