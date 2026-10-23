import XCTest
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
@testable import OttoFlagsSDK

// MARK: - A streaming URLProtocol stub

/// One scripted answer to a stream request: a status and headers, then body chunks, then how
/// the connection ends.
struct StubStreamReply {
    enum Ending {
        /// The body ends normally.
        case finish
        /// The connection stays open until the client stops it.
        case hold
        /// The connection breaks.
        case fail
    }

    var status: Int = 200
    var headers: [String: String] = ["Content-Type": "text/event-stream"]
    var chunks: [Data] = []
    var ending: Ending = .finish

    /// A 200 `text/event-stream` reply whose body is `body`.
    static func events(_ body: String, ending: Ending = .finish) -> StubStreamReply {
        var reply = StubStreamReply()
        reply.chunks = [Data(body.utf8)]
        reply.ending = ending
        return reply
    }

    /// A reply with no body, for refusals.
    static func refusal(_ code: Int, headers: [String: String] = [:]) -> StubStreamReply {
        var reply = StubStreamReply()
        reply.status = code
        reply.headers = headers
        return reply
    }
}

/// One stream request the stub received. Deliveries run on its own serial queue; nothing is sent
/// to the URL loading system after `stopLoading`.
final class StubStreamConnection {
    let request: URLRequest
    private let stub: StreamingURLProtocol
    private let queue = DispatchQueue(label: "com.savvagent.ottoflags.tests.stream-stub")
    private let lock = NSLock()
    private var stopped = false

    init(request: URLRequest, stub: StreamingURLProtocol) {
        self.request = request
        self.stub = stub
    }

    var isStopped: Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }

    func markStopped() {
        lock.lock()
        stopped = true
        lock.unlock()
    }

    /// Runs `body` on the delivery queue unless the client has stopped this connection.
    private func deliver(_ body: @escaping (StreamingURLProtocol) -> Void) {
        queue.async { [self] in
            if self.isStopped {
                return
            }
            body(self.stub)
        }
    }

    func play(_ reply: StubStreamReply) {
        let url = request.url ?? URL(string: "https://stub.invalid")!
        let response = HTTPURLResponse(
            url: url,
            statusCode: reply.status,
            httpVersion: "HTTP/1.1",
            headerFields: reply.headers
        )!
        deliver { stub in
            stub.client?.urlProtocol(stub, didReceive: response, cacheStoragePolicy: .notAllowed)
        }
        for chunk in reply.chunks {
            send(chunk)
        }
        switch reply.ending {
        case .finish:
            finish()
        case .hold:
            break
        case .fail:
            fail()
        }
    }

    func send(_ chunk: Data) {
        deliver { stub in
            stub.client?.urlProtocol(stub, didLoad: chunk)
        }
    }

    func send(_ text: String) {
        send(Data(text.utf8))
    }

    func finish() {
        deliver { stub in
            stub.client?.urlProtocolDidFinishLoading(stub)
        }
    }

    func fail() {
        deliver { stub in
            stub.client?.urlProtocol(stub, didFailWithError: URLError(.networkConnectionLost))
        }
    }
}

/// What the stub answers stream requests with, shared by the `StreamingURLProtocol` instances
/// the loading system creates. Replies are used in order; when none is queued the fallback
/// answers, and without a fallback the request waits for the next queued reply (headers not
/// sent yet).
final class StubStreamRegistry {
    private let lock = NSLock()
    private var connections: [StubStreamConnection] = []
    private var replies: [StubStreamReply] = []
    private var waiting: [StubStreamConnection] = []
    private var fallbackReply: StubStreamReply?

    var fallback: StubStreamReply? {
        get {
            lock.lock()
            defer { lock.unlock() }
            return fallbackReply
        }
        set {
            lock.lock()
            fallbackReply = newValue
            lock.unlock()
        }
    }

    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return connections.count
    }

    func connection(_ index: Int) -> StubStreamConnection {
        lock.lock()
        defer { lock.unlock() }
        return connections[index]
    }

    func accept(_ connection: StubStreamConnection) {
        lock.lock()
        connections.append(connection)
        var reply: StubStreamReply?
        if !replies.isEmpty {
            reply = replies.removeFirst()
        } else if let fallbackReply {
            reply = fallbackReply
        } else {
            waiting.append(connection)
        }
        lock.unlock()
        if let reply {
            connection.play(reply)
        }
    }

    func enqueue(_ reply: StubStreamReply) {
        lock.lock()
        var target: StubStreamConnection?
        while target == nil && !waiting.isEmpty {
            let next = waiting.removeFirst()
            if !next.isStopped {
                target = next
            }
        }
        if target == nil {
            replies.append(reply)
        }
        lock.unlock()
        target?.play(reply)
    }
}

/// A URLProtocol for the stream's session: each request becomes a `StubStreamConnection` in
/// `registry`, which delivers headers, then chunks on demand, and can hold or fail the
/// connection.
final class StreamingURLProtocol: URLProtocol {
    private static let lock = NSLock()
    private static var currentRegistry = StubStreamRegistry()

    static var registry: StubStreamRegistry {
        get {
            lock.lock()
            defer { lock.unlock() }
            return currentRegistry
        }
        set {
            lock.lock()
            currentRegistry = newValue
            lock.unlock()
        }
    }

    private let connectionLock = NSLock()
    private var connection: StubStreamConnection?

    override class func canInit(with request: URLRequest) -> Bool {
        return true
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest {
        return request
    }

    override func startLoading() {
        let connection = StubStreamConnection(request: request, stub: self)
        connectionLock.lock()
        self.connection = connection
        connectionLock.unlock()
        StreamingURLProtocol.registry.accept(connection)
    }

    override func stopLoading() {
        connectionLock.lock()
        let connection = self.connection
        connectionLock.unlock()
        connection?.markStopped()
    }
}

/// Thread-safe test bookkeeping.
final class LockedList<Element> {
    private let lock = NSLock()
    private var items: [Element] = []

    func append(_ item: Element) {
        lock.lock()
        items.append(item)
        lock.unlock()
    }

    var all: [Element] {
        lock.lock()
        defer { lock.unlock() }
        return items
    }
}

// MARK: - Tests

/// The flag-change stream (savvagent/otto-flags#167). Evaluations and the flag list go through
/// the client's session (`MockURLProtocol`): evaluate answers `true` and the flag list lists
/// `checkout`. The stream goes through its own session (`StreamingURLProtocol`).
///
/// These tests use real time with millisecond timings: the stream runs on real queues.
final class RealtimeStreamTests: XCTestCase {
    private var registry = StubStreamRegistry()
    private var clients: [OttoFlagsClient] = []
    private let paths = LockedList<String>()
    private let factoryCalls = LockedList<Int>()

    private let timings = RealtimeTimings(
        baseDelayMillis: 20,
        maxDelayMillis: 40,
        rejectedMaxDelayMillis: 40,
        maxRetryAfterMillis: 40,
        stableAfterMillis: 10_000,
        spreadMillis: 0,
        idleTimeoutMillis: 5_000
    )

    private let user = UserContext(userId: "user-1")

    override func setUp() {
        super.setUp()
        registry = StubStreamRegistry()
        StreamingURLProtocol.registry = registry
        clients = []
        MockURLProtocol.error = nil
        MockURLProtocol.requestHandler = { [paths] request in
            let path = request.url?.path ?? ""
            paths.append(path)
            let body: String
            if path.hasSuffix("/evaluate") {
                body = #"{"enabled": true}"#
            } else if path.hasSuffix("/api/sdk/flags") {
                body = #"{"flags": [{"key": "checkout"}]}"#
            } else {
                body = "{}"
            }
            let response = HTTPURLResponse(
                url: request.url!,
                statusCode: 200,
                httpVersion: nil,
                headerFields: ["Content-Type": "application/json"]
            )!
            return (response, Data(body.utf8))
        }
    }

    override func tearDown() {
        for client in clients {
            client.close()
        }
        clients = []
        MockURLProtocol.requestHandler = nil
        MockURLProtocol.error = nil
        // A fresh registry, so a late request of this test cannot take the next test's replies.
        StreamingURLProtocol.registry = StubStreamRegistry()
        super.tearDown()
    }

    private func streamFactory() -> RealtimeStream.SessionFactory {
        return { [factoryCalls] configuration, delegate, queue in
            factoryCalls.append(1)
            configuration.protocolClasses = [StreamingURLProtocol.self]
            return URLSession(configuration: configuration, delegate: delegate, delegateQueue: queue)
        }
    }

    private func makeClient(
        enableRealtime: Bool = true,
        pollingInterval: TimeInterval = 0,
        apiUrl: String = "https://otto-flags.example.com"
    ) -> OttoFlagsClient {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MockURLProtocol.self]
        let config = OttoFlagsConfig(
            apiUrl: apiUrl,
            sdkKey: "sdk_test-key",
            environment: "test",
            pollingInterval: pollingInterval,
            enableWebSocket: false,
            enableRealtime: enableRealtime
        )
        let client = OttoFlagsClient(
            config: config,
            session: URLSession(configuration: configuration),
            realtimeTimings: timings,
            streamSessionFactory: streamFactory()
        )
        clients.append(client)
        return client
    }

    private func makeStream(sdkKey: String, log: @escaping (String) -> Void) -> RealtimeStream {
        return RealtimeStream(
            url: RealtimeStream.streamURL(apiUrl: "https://otto-flags.example.com")!,
            sdkKey: sdkKey,
            timings: timings,
            sessionFactory: streamFactory(),
            invalidate: { _ in },
            log: log
        )
    }

    private static func event(_ name: String, _ data: String) -> String {
        return "event: \(name)\ndata: \(data)\n\n"
    }

    /// Polls `condition` every 5 ms; fails the test after `timeout` seconds.
    private func waitFor(
        _ what: String,
        timeout: TimeInterval = 5,
        file: StaticString = #filePath,
        line: UInt = #line,
        _ condition: () -> Bool
    ) {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() {
            if Date() > deadline {
                XCTFail("timed out waiting for \(what)", file: file, line: line)
                return
            }
            Thread.sleep(forTimeInterval: 0.005)
        }
    }

    private func pause(_ seconds: TimeInterval) {
        Thread.sleep(forTimeInterval: seconds)
    }

    private func cache(_ client: OttoFlagsClient, _ keys: String...) async throws {
        for key in keys {
            _ = try await client.evaluate(flagKey: key, context: user)
            XCTAssertTrue(client.isCached(flagKey: key), "\(key) cached")
        }
    }

    private func streamPaths() -> [String] {
        return paths.all.filter { $0.hasSuffix("/api/flags/stream") }
    }

    // MARK: The client and the stream

    func testFlagUpdatedDropsTheCachedEvaluation() async throws {
        let client = makeClient()
        waitFor("the stream request") { registry.count == 1 }
        try await cache(client, "checkout", "search")

        registry.enqueue(.events(Self.event("flag.updated", #"{"key":"checkout"}"#), ending: .hold))

        waitFor("checkout invalidated") { !client.isCached(flagKey: "checkout") }
        XCTAssertTrue(client.isCached(flagKey: "search"))
        let again = try await client.evaluate(flagKey: "checkout", context: user)
        XCTAssertEqual(again.reason, "evaluated")
    }

    func testCreatedAndDeletedEventsInvalidateToo() async throws {
        let client = makeClient()
        waitFor("the stream request") { registry.count == 1 }
        try await cache(client, "a", "b")

        registry.enqueue(.events(
            Self.event("flag.created", #"{"key":"a"}"#) + Self.event("flag.deleted", #"{"key":"b"}"#),
            ending: .hold
        ))

        waitFor("both invalidated") { !client.isCached(flagKey: "a") && !client.isCached(flagKey: "b") }
    }

    func testOtherFlagsInvalidKeysAndUnknownEventsLeaveTheCacheAlone() async throws {
        let client = makeClient()
        waitFor("the stream request") { registry.count == 1 }
        try await cache(client, "checkout", "fence")

        let body = [
            Self.event("flag.updated", #"{"key":"other"}"#),
            Self.event("flag.updated", #"{"key":"../checkout"}"#),
            Self.event("flag.updated", #"{"key":""}"#),
            Self.event("flag.updated", #"{"key":42}"#),
            Self.event("flag.updated", "not json"),
            Self.event("flag.updated", #"["checkout"]"#),
            Self.event("heartbeat", "{}"),
            Self.event("mystery", #"{"key":"checkout"}"#),
            "data: {\"key\":\"checkout\"}\n\n",
            ": comment\n\n",
            // Events are applied in order: once the fence is gone, the rest were seen.
            Self.event("flag.updated", #"{"key":"fence"}"#),
        ].joined()
        registry.enqueue(.events(body, ending: .hold))

        waitFor("the fence invalidated") { !client.isCached(flagKey: "fence") }
        XCTAssertTrue(client.isCached(flagKey: "checkout"))
    }

    func testEventsSplitAcrossChunksStillApply() async throws {
        let client = makeClient()
        waitFor("the stream request") { registry.count == 1 }
        try await cache(client, "checkout")

        var reply = StubStreamReply()
        reply.ending = .hold
        registry.enqueue(reply)
        let connection = registry.connection(0)
        connection.send("event: flag.upd")
        connection.send("ated\r\ndata: {\"key\":\"che")
        connection.send("ckout\"}\r\n")
        pause(0.05)
        XCTAssertTrue(client.isCached(flagKey: "checkout"), "not dispatched before the blank line")
        connection.send("\r\n")

        waitFor("checkout invalidated") { !client.isCached(flagKey: "checkout") }
    }

    func testADroppedStreamReconnectsAndConnectedClearsTheCache() async throws {
        let client = makeClient()
        waitFor("the stream request") { registry.count == 1 }
        try await cache(client, "checkout", "search")

        // The first stream ends at once; the client reconnects and waits there.
        registry.enqueue(.events(": hello\n\n"))
        waitFor("a reconnect") { registry.count == 2 }
        XCTAssertTrue(client.isCached(flagKey: "checkout"))

        registry.enqueue(.events(Self.event("connected", "{}"), ending: .hold))
        waitFor("the cache cleared") {
            !client.isCached(flagKey: "checkout") && !client.isCached(flagKey: "search")
        }
    }

    func testABrokenConnectionIsRetried() async throws {
        registry.fallback = .events(": hi\n\n", ending: .fail)
        let client = makeClient()
        waitFor("stream retries") { registry.count >= 3 }
        try await cache(client, "checkout")
    }

    func testWhileTheStreamIsRefusedLookupsWorkAndThePollStillRefreshes() async throws {
        registry.fallback = .refusal(503, headers: ["Retry-After": "1"])
        // No timer: a background poll could outlive tearDown and hit a cleared handler, so the
        // poll is run by hand, which is the same invalidation.
        let client = makeClient()
        waitFor("stream retries") { registry.count >= 3 }

        try await cache(client, "checkout")
        await client.refreshFlags()
        XCTAssertFalse(client.isCached(flagKey: "checkout"))
        XCTAssertFalse(paths.all.filter { $0.hasSuffix("/api/sdk/flags") }.isEmpty)
        let again = try await client.evaluate(flagKey: "checkout", context: user)
        XCTAssertTrue(again.value)
    }

    func testAStreamThatIsNotAnEventStreamIsRetried() async throws {
        var reply = StubStreamReply()
        reply.headers = ["Content-Type": "application/json"]
        reply.chunks = [Data(Self.event("connected", "{}").utf8)]
        registry.fallback = reply
        let client = makeClient()
        waitFor("stream retries") { registry.count >= 3 }

        try await cache(client, "checkout")
        pause(0.05)
        // The body of a refused response is never parsed.
        XCTAssertTrue(client.isCached(flagKey: "checkout"))
    }

    func testNoStreamRequestWhenRealtimeIsOff() async throws {
        XCTAssertFalse(OttoFlagsConfig(sdkKey: "k").enableRealtime)
        let client = makeClient(enableRealtime: false)
        try await cache(client, "checkout")
        pause(0.3)

        XCTAssertEqual(registry.count, 0)
        XCTAssertTrue(factoryCalls.all.isEmpty)
        XCTAssertTrue(streamPaths().isEmpty)
        XCTAssertEqual(paths.all.count, 1)
    }

    func testALookupIsPromptWhileTheStreamIsHeld() async throws {
        registry.enqueue(.events(Self.event("connected", "{}"), ending: .hold))
        let client = makeClient()
        waitFor("the stream request") { registry.count == 1 }

        let started = Date()
        let result = try await client.evaluate(flagKey: "checkout", context: user)
        let took = Date().timeIntervalSince(started)

        XCTAssertTrue(result.value)
        XCTAssertLessThan(took, 1.0, "lookup took \(took) s")
    }

    func testTheStreamRequestCarriesTheKeyInTheHeaderAndNoQuery() {
        _ = makeClient(apiUrl: "https://otto-flags.example.com/prefix/?token=secret#frag")
        waitFor("the stream request") { registry.count == 1 }

        let request = registry.connection(0).request
        XCTAssertEqual(request.httpMethod, "GET")
        XCTAssertEqual(request.url?.path, "/prefix/api/flags/stream")
        XCTAssertNil(request.url?.query)
        XCTAssertNil(request.url?.fragment)
        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer sdk_test-key")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Accept"), "text/event-stream")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Accept-Encoding"), "identity")
        XCTAssertFalse(request.url?.absoluteString.contains("sdk_test-key") ?? true)
    }

    func testARedirectStatusIsARefusedAttempt() {
        registry.fallback = .refusal(307, headers: ["Location": "https://elsewhere.example/steal"])
        _ = makeClient()
        waitFor("stream retries") { registry.count >= 3 }

        for index in 0..<3 {
            XCTAssertEqual(registry.connection(index).request.url?.host, "otto-flags.example.com")
        }
        XCTAssertTrue(paths.all.isEmpty, "no request went through the client session")
    }

    func testTheDelegateRefusesToFollowARedirect() {
        let delegate = RealtimeStreamDelegate()
        let session = URLSession(configuration: .ephemeral)
        defer { session.invalidateAndCancel() }
        let task = session.dataTask(with: URL(string: "https://otto-flags.example.com/api/flags/stream")!)
        let response = HTTPURLResponse(
            url: URL(string: "https://otto-flags.example.com/api/flags/stream")!,
            statusCode: 307,
            httpVersion: "HTTP/1.1",
            headerFields: ["Location": "https://elsewhere.example/steal"]
        )!
        var newRequest = URLRequest(url: URL(string: "https://elsewhere.example/steal")!)
        newRequest.setValue("Bearer sdk_test-key", forHTTPHeaderField: "Authorization")

        let decided = LockedList<URLRequest?>()
        delegate.urlSession(
            session,
            task: task,
            willPerformHTTPRedirection: response,
            newRequest: newRequest
        ) { request in
            decided.append(request)
        }

        XCTAssertEqual(decided.all.count, 1)
        XCTAssertNil(decided.all.first ?? nil)
    }

    func testCloseStopsReconnecting() {
        registry.fallback = .refusal(503)
        let client = makeClient()
        waitFor("stream retries") { registry.count >= 2 }

        client.close()
        pause(0.1)
        let after = registry.count
        pause(0.3)

        XCTAssertEqual(registry.count, after)
        client.close() // idempotent
    }

    func testCloseCancelsAHeldConnection() {
        registry.enqueue(.events(Self.event("connected", "{}"), ending: .hold))
        let client = makeClient()
        waitFor("the stream request") { registry.count == 1 }
        pause(0.05)
        XCTAssertFalse(registry.connection(0).isStopped)

        client.close()

        waitFor("the connection stopped", timeout: 2) { registry.connection(0).isStopped }
        pause(0.2)
        XCTAssertEqual(registry.count, 1)
    }

    func testReleasingTheClientStopsTheStream() {
        // Headers only: no event calls back into the client while it is released.
        var reply = StubStreamReply()
        reply.ending = .hold
        registry.enqueue(reply)
        var client: OttoFlagsClient? = OttoFlagsClient(
            config: OttoFlagsConfig(
                apiUrl: "https://otto-flags.example.com",
                sdkKey: "sdk_test-key",
                pollingInterval: 0,
                enableWebSocket: false,
                enableRealtime: true
            ),
            session: URLSession(configuration: .ephemeral),
            realtimeTimings: timings,
            streamSessionFactory: streamFactory()
        )
        waitFor("the stream request") { registry.count == 1 }
        weak var released = client
        client = nil

        XCTAssertNil(released, "nothing else holds the client")
        waitFor("the connection stopped", timeout: 2) { registry.connection(0).isStopped }
    }

    func testAnUnusableApiUrlStartsNoStream() async {
        let client = makeClient(apiUrl: "ftp://example.invalid")
        _ = try? await client.evaluate(flagKey: "checkout", context: user)
        pause(0.1)

        XCTAssertNil(RealtimeStream.streamURL(apiUrl: "ftp://example.invalid"))
        XCTAssertTrue(factoryCalls.all.isEmpty)
        XCTAssertEqual(registry.count, 0)
    }

    // MARK: RealtimeStream on its own

    func testFailuresAreLoggedOncePerRunWithoutTheKey() {
        registry.fallback = .refusal(401)
        let logged = LockedList<String>()
        let stream = makeStream(sdkKey: "sdk_secret-key") { logged.append($0) }
        stream.start()
        waitFor("stream retries") { registry.count >= 4 }
        stream.stop()

        let messages = logged.all
        XCTAssertEqual(messages.count, 1, "\(messages)")
        XCTAssertTrue(messages.first?.contains("status 401") ?? false, "\(messages)")
        XCTAssertFalse(messages.contains { $0.contains("secret") })
    }

    func testAConnectionLogsAgainAfterAnOpenStream() {
        let logged = LockedList<String>()
        registry.enqueue(.refusal(500))
        registry.enqueue(.events(": up\n\n"))
        registry.enqueue(.refusal(502))
        registry.fallback = .events(": up\n\n", ending: .hold)
        let stream = makeStream(sdkKey: "sdk_test-key") { logged.append($0) }
        stream.start()
        waitFor("the held connection") { registry.count >= 4 }
        stream.stop()

        let messages = logged.all
        XCTAssertEqual(messages.count, 2, "\(messages)")
        XCTAssertTrue(messages.first?.contains("status 500") ?? false, "\(messages)")
        XCTAssertTrue(messages.last?.contains("status 502") ?? false, "\(messages)")
    }

    func testStopBeforeStartOpensNothing() {
        let stream = makeStream(sdkKey: "sdk_test-key") { _ in }
        stream.stop()
        stream.start()
        stream.stop()
        pause(0.1)

        XCTAssertEqual(registry.count, 0)
        XCTAssertTrue(factoryCalls.all.isEmpty)
    }

    // MARK: Pure functions

    func testParserSplitsEventsAcrossEveryChunkBoundary() throws {
        let lines: [String] = [
            ": heartbeat\n",
            "event: connected\r\n",
            "data: {}\r\n",
            "\r\n",
            "event: flag.updated\n",
            "data: {\"key\":\n",
            "data:\"caf\u{E9}\"}\n",
            "id: 7\n",
            "retry: 10\n",
            "\n",
            "data\n",
            "\n",
            "event: tail\n",
        ]
        let text = lines.joined()
        let body = [UInt8](text.utf8)
        let expected = [
            SSEEvent(event: "connected", data: "{}"),
            SSEEvent(event: "flag.updated", data: "{\"key\":\n\"caf\u{E9}\"}"),
            SSEEvent(event: "message", data: ""),
        ]

        for split in 0...body.count {
            var parser = SSEParser()
            var got = try parser.feed(Data(body[0..<split]))
            got += try parser.feed(Data(body[split..<body.count]))
            XCTAssertEqual(got, expected, "split at \(split)")
        }

        var parser = SSEParser()
        var byteByByte: [SSEEvent] = []
        for byte in body {
            byteByByte += try parser.feed(Data([byte]))
        }
        XCTAssertEqual(byteByByte, expected)
    }

    func testParserDropsAnEventWithNoFields() throws {
        var parser = SSEParser()
        XCTAssertEqual(try parser.feed(Data("\n\n: x\n\nid: 1\n\n".utf8)), [])
        XCTAssertEqual(try parser.feed(Data("event: e\n\n".utf8)), [SSEEvent(event: "e", data: "")])
        XCTAssertEqual(try parser.feed(Data("data:a:b\n\n".utf8)), [SSEEvent(event: "message", data: "a:b")])
    }

    private static func feedThrowsTooLarge(_ parser: inout SSEParser, _ text: String) -> Bool {
        do {
            _ = try parser.feed(Data(text.utf8))
            return false
        } catch is SSEEventTooLarge {
            return true
        } catch {
            return false
        }
    }

    func testParserRefusesAnEventOverTheCap() throws {
        var parser = SSEParser(maxEventBytes: 16)
        XCTAssertFalse(Self.feedThrowsTooLarge(&parser, "data: 0123456789\n"))
        XCTAssertTrue(Self.feedThrowsTooLarge(&parser, "data: 0123456789\n"))

        var unterminated = SSEParser(maxEventBytes: 16)
        XCTAssertTrue(Self.feedThrowsTooLarge(&unterminated, "data: " + String(repeating: "x", count: 32)))

        // Empty data lines cost a byte each.
        var empties = SSEParser(maxEventBytes: 16)
        XCTAssertTrue(Self.feedThrowsTooLarge(&empties, String(repeating: "data:\n", count: 40)))

        // A dispatched event frees its budget.
        var reused = SSEParser(maxEventBytes: 16)
        for _ in 0..<10 {
            XCTAssertEqual(try reused.feed(Data("data: 0123456789\n\n".utf8)).count, 1)
        }
    }

    func testReconnectDelayDoublesToTheCapWithJitterInTheUpperHalf() {
        XCTAssertEqual(RealtimeStream.reconnectDelay(baseMillis: 1_000, attempt: 0, ceilingMillis: 30_000, unit: 0), 500)
        XCTAssertEqual(RealtimeStream.reconnectDelay(baseMillis: 1_000, attempt: 0, ceilingMillis: 30_000, unit: 1), 1_000)
        XCTAssertEqual(RealtimeStream.reconnectDelay(baseMillis: 1_000, attempt: 3, ceilingMillis: 30_000, unit: 0.5), 6_000)
        XCTAssertEqual(RealtimeStream.reconnectDelay(baseMillis: 1_000, attempt: 2, ceilingMillis: 30_000, unit: 0), 2_000)
        XCTAssertEqual(RealtimeStream.reconnectDelay(baseMillis: 1_000, attempt: 64, ceilingMillis: 30_000, unit: 0), 15_000)
        XCTAssertEqual(RealtimeStream.reconnectDelay(baseMillis: 1_000, attempt: Int.max, ceilingMillis: 30_000, unit: 1), 30_000)
        for attempt in 0...70 {
            for unit in [0.0, 0.25, 0.5, 0.999, -1.0, 2.0, Double.nan] {
                let delay = RealtimeStream.reconnectDelay(baseMillis: 1_000, attempt: attempt, ceilingMillis: 30_000, unit: unit)
                XCTAssertTrue((500...30_000).contains(delay), "attempt \(attempt) unit \(unit): \(delay)")
            }
        }
        XCTAssertEqual(RealtimeStream.reconnectDelay(baseMillis: 0, attempt: 3, ceilingMillis: 30_000, unit: 0.5), 0)
    }

    func testReconnectSpreadAppliesOnlyToTheFirstRetryOfAnEstablishedStream() {
        XCTAssertEqual(RealtimeStream.reconnectSpread(spreadMillis: 5_000, attempt: 0, wasUp: true, unit: 0.5), 2_500)
        XCTAssertEqual(RealtimeStream.reconnectSpread(spreadMillis: 5_000, attempt: 1, wasUp: true, unit: 0.5), 0)
        XCTAssertEqual(RealtimeStream.reconnectSpread(spreadMillis: 5_000, attempt: 0, wasUp: false, unit: 0.5), 0)
        XCTAssertEqual(RealtimeStream.reconnectSpread(spreadMillis: 5_000, attempt: 0, wasUp: true, unit: Double.nan), 0)
        XCTAssertEqual(RealtimeStream.reconnectSpread(spreadMillis: 5_000, attempt: 0, wasUp: true, unit: 7), 5_000)
    }

    func testNextDelayCapsRejectionsLongerAndHonoursRetryAfter() {
        let t = RealtimeTimings(random: { 1.0 })
        XCTAssertEqual(t.nextDelay(end: .refused, status: 500, retryAfterMillis: -1, attempt: 20), 30_000)
        XCTAssertEqual(t.nextDelay(end: .refused, status: 401, retryAfterMillis: -1, attempt: 20), 300_000)
        XCTAssertEqual(t.nextDelay(end: .refused, status: 403, retryAfterMillis: -1, attempt: 20), 300_000)
        XCTAssertEqual(t.nextDelay(end: .refused, status: 404, retryAfterMillis: -1, attempt: 20), 300_000)
        // Retry-After lengthens, is capped, and never shortens.
        XCTAssertEqual(t.nextDelay(end: .refused, status: 503, retryAfterMillis: 42_000, attempt: 0), 42_000)
        XCTAssertEqual(t.nextDelay(end: .refused, status: 429, retryAfterMillis: 3_600_000, attempt: 0), 60_000)
        XCTAssertEqual(t.nextDelay(end: .refused, status: 503, retryAfterMillis: 1_000, attempt: 20), 30_000)
        XCTAssertEqual(t.nextDelay(end: .refused, status: 500, retryAfterMillis: 42_000, attempt: 0), 1_000)
        // The first retry after an established stream adds the spread.
        XCTAssertEqual(t.nextDelay(end: .dropped, status: 0, retryAfterMillis: -1, attempt: 0), 6_000)
        XCTAssertEqual(t.nextDelay(end: .unreachable, status: 0, retryAfterMillis: -1, attempt: 0), 1_000)
    }

    func testParseRetryAfter() {
        XCTAssertEqual(RealtimeStream.parseRetryAfter(nil), -1)
        XCTAssertEqual(RealtimeStream.parseRetryAfter(""), -1)
        XCTAssertEqual(RealtimeStream.parseRetryAfter("-1"), -1)
        XCTAssertEqual(RealtimeStream.parseRetryAfter("1.5"), -1)
        XCTAssertEqual(RealtimeStream.parseRetryAfter("Wed, 21 Oct 2026 07:28:00 GMT"), -1)
        XCTAssertEqual(RealtimeStream.parseRetryAfter("0"), 0)
        XCTAssertEqual(RealtimeStream.parseRetryAfter(" 120 "), 120_000)
        XCTAssertEqual(RealtimeStream.parseRetryAfter("99999999999999999999999"), Int.max)
    }

    func testIsEventStream() {
        XCTAssertTrue(RealtimeStream.isEventStream("text/event-stream"))
        XCTAssertTrue(RealtimeStream.isEventStream("Text/Event-Stream; charset=utf-8"))
        XCTAssertFalse(RealtimeStream.isEventStream("application/json"))
        XCTAssertFalse(RealtimeStream.isEventStream("text/event-stream-ish"))
        XCTAssertFalse(RealtimeStream.isEventStream(nil))
    }

    func testStreamURLKeepsAPathPrefixAndDropsTheQuery() {
        XCTAssertEqual(RealtimeStream.streamURL(apiUrl: "https://h.example")?.absoluteString, "https://h.example/api/flags/stream")
        XCTAssertEqual(RealtimeStream.streamURL(apiUrl: "https://h.example/")?.absoluteString, "https://h.example/api/flags/stream")
        XCTAssertEqual(
            RealtimeStream.streamURL(apiUrl: "https://h.example/p/?a=b#c")?.absoluteString,
            "https://h.example/p/api/flags/stream"
        )
        XCTAssertEqual(
            RealtimeStream.streamURL(apiUrl: "https://user:pw@h.example")?.absoluteString,
            "https://h.example/api/flags/stream"
        )
        XCTAssertEqual(
            RealtimeStream.streamURL(apiUrl: "http://h.example:8080")?.absoluteString,
            "http://h.example:8080/api/flags/stream"
        )
        XCTAssertNil(RealtimeStream.streamURL(apiUrl: "not a url"))
        XCTAssertNil(RealtimeStream.streamURL(apiUrl: ""))
        XCTAssertNil(RealtimeStream.streamURL(apiUrl: "ftp://h.example"))
    }

    func testApplyEventMapsEventsToInvalidations() {
        func applied(_ name: String, _ data: String) -> [String?] {
            var out: [String?] = []
            RealtimeStream.applyEvent(SSEEvent(event: name, data: data)) { out.append($0) }
            return out
        }
        XCTAssertEqual(applied("connected", ""), [nil])
        XCTAssertEqual(applied("flag.updated", #"{"key":"a.b-c_1","version":3}"#), ["a.b-c_1"])
        XCTAssertEqual(applied("flag.created", #"{"key":"a"}"#), ["a"])
        XCTAssertEqual(applied("flag.deleted", #"{"key":"a"}"#), ["a"])
        XCTAssertEqual(applied("flag.updated", #"{"key":".."}"#), [])
        XCTAssertEqual(applied("flag.updated", #"{"key":"a/b"}"#), [])
        XCTAssertEqual(applied("flag.updated", #"{"key":null}"#), [])
        XCTAssertEqual(applied("flag.updated", "{\"key\":\"" + String(repeating: "k", count: 129) + "\"}"), [])
        XCTAssertEqual(applied("heartbeat", "{}"), [])
        XCTAssertEqual(applied("message", #"{"key":"a"}"#), [])
    }
}
