import XCTest
@testable import OttoFlagsSDK

/// Flag keys are validated before they reach a URL (savvagent/otto-flags#107).
final class FlagKeyValidationTests: XCTestCase {
    private var client: OttoFlagsClient!
    private let lock = NSLock()
    private var urls: [URL] = []

    override func setUp() {
        super.setUp()
        urls = []
        MockURLProtocol.error = nil
        MockURLProtocol.requestHandler = { [weak self] request in
            if let url = request.url {
                self?.record(url)
            }
            let response = HTTPURLResponse(
                url: request.url!,
                statusCode: 200,
                httpVersion: nil,
                headerFields: nil
            )!
            let body = #"{"enabled": true, "value": "on"}"#.data(using: .utf8)!
            return (response, body)
        }

        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MockURLProtocol.self]
        let session = URLSession(configuration: configuration)

        let config = OttoFlagsConfig(
            apiUrl: "https://otto-flags.example.com",
            sdkKey: "sdk_test",
            environment: "test",
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

    private func record(_ url: URL) {
        lock.lock()
        urls.append(url)
        lock.unlock()
    }

    /// Requests to the per-flag endpoints only (polling may fetch other paths).
    private func flagRequests() -> [URL] {
        lock.lock()
        defer { lock.unlock() }
        return urls.filter { $0.path.hasSuffix("/evaluate") || $0.path.hasSuffix("/variation") }
    }

    private var invalidKeys: [String] {
        ["a/b", "a?b", "a#b", ".", "..", "", "bad key", String(repeating: "a", count: 129)]
    }

    func testEvaluateRejectsInvalidKeysWithoutARequest() async {
        for key in invalidKeys {
            do {
                _ = try await client.evaluate(flagKey: key, context: UserContext(userId: "u"))
                XCTFail("expected invalidFlagKey for key of length \(key.count)")
            } catch OttoFlagsError.invalidFlagKey {
                // expected
            } catch {
                XCTFail("unexpected error \(error) for key of length \(key.count)")
            }
        }
        XCTAssertEqual(flagRequests().count, 0)
    }

    func testGetVariationReturnsDefaultForInvalidKeysWithoutARequest() async {
        for key in invalidKeys {
            let value = await client.getVariation(
                flagKey: key,
                context: UserContext(userId: "u"),
                defaultValue: "fallback"
            )
            XCTAssertEqual(value, "fallback")
        }
        XCTAssertEqual(flagRequests().count, 0)
    }

    func testValidKeyKeepsTheExistingEvaluatePath() async throws {
        let result = try await client.evaluate(
            flagKey: "new-checkout.v2_b",
            context: UserContext(userId: "u")
        )
        XCTAssertTrue(result.value)
        let requests = flagRequests()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.path, "/v1/flags/new-checkout.v2_b/evaluate")
    }

    func testValidKeyKeepsTheExistingVariationPath() async {
        let value = await client.getVariation(
            flagKey: "new-checkout.v2_b",
            context: UserContext(userId: "u"),
            defaultValue: "fallback"
        )
        XCTAssertEqual(value, "on")
        XCTAssertEqual(flagRequests().first?.path, "/v1/flags/new-checkout.v2_b/variation")
    }

    func testInvalidKeyErrorDescribesTheKeyBriefly() async {
        let key = String(repeating: "a", count: 200) + "/"
        do {
            _ = try await client.evaluate(flagKey: key, context: UserContext(userId: "u"))
            XCTFail("expected invalidFlagKey")
        } catch OttoFlagsError.invalidFlagKey(let message) {
            XCTAssertTrue(message.contains("201"))
            XCTAssertFalse(message.contains(String(repeating: "a", count: 65)))
        } catch {
            XCTFail("unexpected error \(error)")
        }
    }
}
