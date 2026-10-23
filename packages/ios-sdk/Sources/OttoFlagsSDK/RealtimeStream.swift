import Foundation
import Dispatch
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// One server-sent event: its `event` field ("message" when it had none) and its `data`.
struct SSEEvent: Equatable {
    let event: String
    let data: String
}

/// An event (its pending line plus its data) grew past `SSEParser.maxEventBytes`.
struct SSEEventTooLarge: Error {
    let limit: Int
}

/// An incremental `text/event-stream` parser: feed it the body in chunks of any size, split
/// anywhere (inside a multi-byte character included), and it returns the events each chunk
/// completed.
///
/// Bytes are buffered and a line is decoded only once it is complete, so a UTF-8 character split
/// across chunks survives. Lines end in `\n` or `\r\n`; lines starting with `:` are comments (the
/// server's heartbeats); several `data` lines join with `\n`; an event with neither an `event`
/// nor a `data` line is dropped; `id` and `retry` are ignored (a reconnect clears the cache
/// instead of resuming, and the delay is the client's own). The pending line plus the pending
/// data may not exceed `maxEventBytes`, so a peer cannot grow the buffers without bound. Not
/// thread safe: one parser per connection.
struct SSEParser {
    /// 4 MiB: far above any real event, which carries one flag key.
    static let defaultMaxEventBytes = 4 * 1024 * 1024

    let maxEventBytes: Int
    /// The bytes of the line not yet terminated.
    private var line: [UInt8] = []
    private var event: String?
    private var data = ""
    /// Bytes in `data`, which is what the cap counts.
    private var dataBytes = 0
    private var hasData = false

    init(maxEventBytes: Int = SSEParser.defaultMaxEventBytes) {
        self.maxEventBytes = maxEventBytes
    }

    /// Feeds `chunk` and returns the events it completed.
    /// - Throws: `SSEEventTooLarge` when the pending event passes the cap.
    mutating func feed(_ chunk: Data) throws -> [SSEEvent] {
        var events: [SSEEvent] = []
        let bytes = [UInt8](chunk)
        var start = 0
        var index = 0
        while index < bytes.count {
            if bytes[index] == 0x0A {
                line.append(contentsOf: bytes[start..<index])
                try checkSize()
                var complete = line
                line.removeAll(keepingCapacity: true)
                if complete.last == 0x0D {
                    complete.removeLast()
                }
                if let event = try process(complete) {
                    events.append(event)
                }
                start = index + 1
            }
            index += 1
        }
        line.append(contentsOf: bytes[start..<bytes.count])
        try checkSize()
        return events
    }

    private func checkSize() throws {
        if line.count + dataBytes > maxEventBytes {
            throw SSEEventTooLarge(limit: maxEventBytes)
        }
    }

    /// Applies one complete line (without its terminator); an empty line dispatches the pending
    /// event. Fields are split on bytes, so a `:` is never mistaken inside a grapheme cluster.
    private mutating func process(_ bytes: [UInt8]) throws -> SSEEvent? {
        if bytes.isEmpty {
            let name = event
            let had = hasData
            let value = data
            event = nil
            data = ""
            dataBytes = 0
            hasData = false
            if name == nil && !had {
                return nil
            }
            return SSEEvent(event: name ?? "message", data: value)
        }
        if bytes[0] == 0x3A {
            return nil
        }
        let field: String
        var valueBytes: ArraySlice<UInt8>
        if let colon = bytes.firstIndex(of: 0x3A) {
            field = String(decoding: bytes[0..<colon], as: UTF8.self)
            valueBytes = bytes[(colon + 1)..<bytes.count]
            if valueBytes.first == 0x20 {
                valueBytes = valueBytes.dropFirst()
            }
        } else {
            field = String(decoding: bytes, as: UTF8.self)
            valueBytes = []
        }
        switch field {
        case "event":
            event = String(decoding: valueBytes, as: UTF8.self)
        case "data":
            if hasData {
                data += "\n"
                dataBytes += 1
            }
            data += String(decoding: valueBytes, as: UTF8.self)
            dataBytes += valueBytes.count
            hasData = true
            if dataBytes > maxEventBytes {
                throw SSEEventTooLarge(limit: maxEventBytes)
            }
        default:
            break
        }
        return nil
    }
}

/// How one connection attempt ended.
enum StreamEnd {
    /// The request failed before a response (refused, unreachable, timeout).
    case unreachable
    /// A non-2xx status (a redirect included), or a 2xx that is not `text/event-stream`.
    case refused
    /// The stream was open, then ended (closed, broke, too large, or went idle).
    case dropped
}

/// The stream's timings, in milliseconds. The client uses `standard`; tests pass small values so
/// nothing waits for seconds.
struct RealtimeTimings {
    /// The first retry's full delay; the wait is half of it to all of it.
    var baseDelayMillis: Int = 1_000
    /// Caps the delay after a dropped stream or a transport error.
    var maxDelayMillis: Int = 30_000
    /// Caps the delay after a 401, 403 or 404: the key is refused or the endpoint absent, and
    /// retrying fast changes nothing but the battery.
    var rejectedMaxDelayMillis: Int = 300_000
    /// Caps how far a Retry-After can stretch one wait, so a proxy cannot hold off for hours.
    var maxRetryAfterMillis: Int = 60_000
    /// How long a connection must stay up to reset the backoff.
    var stableAfterMillis: Int = 30_000
    /// The longest extra wait before the first retry of a stream that was up, so the clients
    /// whose streams one deploy dropped do not reconnect together.
    var spreadMillis: Int = 5_000
    /// The session's request timeout, which for a data task is the longest gap between packets:
    /// the server sends a heartbeat every 30 s, so three missed ones mean the connection is dead
    /// even if the socket has not noticed (a phone that changed networks, say).
    var idleTimeoutMillis: Int = 90_000
    /// Returns a value in [0, 1) for jitter. Must be safe for concurrent use.
    var random: () -> Double = { Double.random(in: 0..<1) }

    static let standard = RealtimeTimings()

    /// The wait after a connection that ended as `end` before attempt `attempt` (from 0).
    /// `status` and `retryAfterMillis` (-1 when absent) are those of a refused attempt.
    func nextDelay(end: StreamEnd, status: Int, retryAfterMillis: Int, attempt: Int) -> Int {
        let rejected = end == .refused && (status == 401 || status == 403 || status == 404)
        let ceiling = rejected ? rejectedMaxDelayMillis : maxDelayMillis
        var delay = RealtimeStream.reconnectDelay(
            baseMillis: baseDelayMillis, attempt: attempt, ceilingMillis: ceiling, unit: random()
        )
        delay = RealtimeStream.saturatedAdd(
            delay,
            RealtimeStream.reconnectSpread(
                spreadMillis: spreadMillis, attempt: attempt, wasUp: end == .dropped, unit: random()
            )
        )
        if end == .refused && retryAfterMillis >= 0 && (status == 429 || status == 503) {
            // Retry-After only ever lengthens the wait.
            delay = max(delay, min(retryAfterMillis, maxRetryAfterMillis))
        }
        return delay
    }
}

/// The flag-change stream, `GET /api/flags/stream` (docs/SDK-DEVELOPER-GUIDE.md, Real-time
/// Updates), which invalidates cached evaluations so a flag change, a kill switch above all,
/// reaches the app in about a round trip instead of at the next poll (savvagent/otto-flags#167).
///
/// One long-lived data task at a time on a dedicated `URLSession` whose `URLSessionDataDelegate`
/// receives the body as it arrives (`URLSession.bytes` needs iOS 15; the package declares iOS
/// 13). A `flag.created`, `flag.updated` or `flag.deleted` event invalidates that flag through
/// the client's generation counters, so an evaluation in flight when it arrived is not cached
/// either (#137). Each `connected` event (the server sends one on every connect) invalidates the
/// whole cache: events may have been missed while the stream was down. Lookups never wait on the
/// stream: it only invalidates, and polling keeps running as the fallback.
///
/// A dropped or refused stream is retried with exponential backoff and jitter
/// (`RealtimeTimings.nextDelay`); the attempt count resets only after a connection stayed up for
/// `stableAfterMillis`, so a server that accepts and drops at once is not retried at the base
/// delay forever. The first failed attempt of each run of failures is passed to `log` (a run ends
/// when a connection opens). Nothing is thrown into the host. The key is sent only in the
/// `Authorization` header, never in the URL or a log line, and a redirect is refused so the key
/// does not follow it.
///
/// The session retains its delegate strongly; the delegate holds the stream weakly, and the
/// stream invalidates the session in `stop()` and `deinit`, so nothing leaks. All mutable state is
/// guarded by `lock`; the invalidation callback and `log` are called without it held.
final class RealtimeStream: @unchecked Sendable {
    /// Builds the stream's session from the configuration the stream prepared, its delegate and
    /// its serial delegate queue. Tests add `protocolClasses` to the configuration.
    typealias SessionFactory = (URLSessionConfiguration, URLSessionDataDelegate, OperationQueue) -> URLSession

    static let defaultSessionFactory: SessionFactory = { configuration, delegate, queue in
        URLSession(configuration: configuration, delegate: delegate, delegateQueue: queue)
    }

    /// Bounds the attempt counter; the delay reaches its cap long before.
    static let maxAttempt = 64

    private let request: URLRequest
    private let timings: RealtimeTimings
    private let sessionFactory: SessionFactory
    private let invalidate: (String?) -> Void
    private let log: (String) -> Void
    private let delegate = RealtimeStreamDelegate()
    private let delegateQueue: OperationQueue
    private let timerQueue = DispatchQueue(label: "com.savvagent.ottoflags.sdk.realtime")

    private let lock = NSLock()
    // Guarded by `lock`.
    private var stopped = false
    private var session: URLSession?
    private var task: URLSessionDataTask?
    private var attempt = 0
    private var failureLogged = false
    // The current attempt's state, also guarded by `lock`.
    private var parser = SSEParser()
    private var responseSeen = false
    /// When the current response was accepted (uptime nanoseconds); nil until then.
    private var openedAt: UInt64?
    private var refused = false
    private var refusedStatus = 0
    private var retryAfterMillis = -1

    /// - Parameters:
    ///   - url: the stream URL (`RealtimeStream.streamURL(apiUrl:)`).
    ///   - invalidate: drops one flag's cached evaluations, or every flag's for nil.
    ///   - log: receives failure descriptions; never given the key.
    init(
        url: URL,
        sdkKey: String,
        timings: RealtimeTimings = .standard,
        sessionFactory: @escaping SessionFactory = RealtimeStream.defaultSessionFactory,
        invalidate: @escaping (String?) -> Void,
        log: @escaping (String) -> Void = RealtimeStream.debugLog
    ) {
        var request = URLRequest(url: url)
        request.httpMethod = "GET"
        request.setValue("Bearer \(sdkKey)", forHTTPHeaderField: "Authorization")
        request.setValue("text/event-stream", forHTTPHeaderField: "Accept")
        // Transparent gzip could hold events back behind a proxy that compresses without
        // flushing each one.
        request.setValue("identity", forHTTPHeaderField: "Accept-Encoding")
        request.cachePolicy = .reloadIgnoringLocalCacheData
        request.timeoutInterval = Double(timings.idleTimeoutMillis) / 1000
        self.request = request
        self.timings = timings
        self.sessionFactory = sessionFactory
        self.invalidate = invalidate
        self.log = log
        let queue = OperationQueue()
        queue.maxConcurrentOperationCount = 1
        queue.name = "com.savvagent.ottoflags.sdk.realtime.delegate"
        self.delegateQueue = queue
    }

    deinit {
        // `stop()` clears `session`; this covers a stream released without it.
        session?.invalidateAndCancel()
    }

    /// Opens the session and the first connection. Does nothing once started or stopped.
    func start() {
        lock.lock()
        if stopped || session != nil {
            lock.unlock()
            return
        }
        delegate.stream = self
        session = sessionFactory(makeConfiguration(), delegate, delegateQueue)
        lock.unlock()
        connect()
    }

    /// Cancels the connection and any pending reconnect, and invalidates the session. Idempotent;
    /// a stopped stream does not start again.
    func stop() {
        lock.lock()
        if stopped {
            lock.unlock()
            return
        }
        stopped = true
        let session = self.session
        let task = self.task
        self.session = nil
        self.task = nil
        lock.unlock()
        task?.cancel()
        session?.invalidateAndCancel()
    }

    private func makeConfiguration() -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.default
        configuration.timeoutIntervalForRequest = Double(timings.idleTimeoutMillis) / 1000
        // No total timeout worth the name, which would cut the stream off.
        configuration.timeoutIntervalForResource = 60 * 60 * 24 * 365
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.urlCache = nil
        return configuration
    }

    private func connect() {
        lock.lock()
        // Checked under the lock `stop()` sets `stopped` under, so no task is ever created on an
        // invalidated session (which would raise an exception).
        guard !stopped, let session = self.session else {
            lock.unlock()
            return
        }
        let task = session.dataTask(with: request)
        self.task = task
        parser = SSEParser()
        responseSeen = false
        openedAt = nil
        refused = false
        refusedStatus = 0
        retryAfterMillis = -1
        lock.unlock()
        task.resume()
    }

    /// Records the response of the current attempt and returns whether to read its body. Call
    /// with `lock` held.
    private func classifyLocked(_ response: URLResponse?) -> Bool {
        responseSeen = true
        guard let http = response as? HTTPURLResponse else {
            refused = true
            return false
        }
        let status = http.statusCode
        guard (200...299).contains(status) else {
            refused = true
            refusedStatus = status
            retryAfterMillis = RealtimeStream.parseRetryAfter(http.value(forHTTPHeaderField: "Retry-After"))
            return false
        }
        guard RealtimeStream.isEventStream(http.value(forHTTPHeaderField: "Content-Type")) else {
            refused = true
            refusedStatus = status
            return false
        }
        openedAt = DispatchTime.now().uptimeNanoseconds
        return true
    }

    fileprivate func accept(_ response: URLResponse, task: URLSessionTask) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !stopped, task === self.task, !responseSeen else {
            return false
        }
        return classifyLocked(response)
    }

    fileprivate func receive(_ data: Data, task: URLSessionDataTask) {
        var events: [SSEEvent] = []
        var cancel = false
        lock.lock()
        guard !stopped, task === self.task else {
            lock.unlock()
            return
        }
        if !responseSeen {
            // In case the response callback was not delivered first.
            cancel = !classifyLocked(task.response)
        }
        if !cancel {
            if openedAt == nil {
                // The body of a refused response.
                lock.unlock()
                return
            }
            do {
                events = try parser.feed(data)
            } catch {
                // Too large: end this connection; the reconnect starts a fresh parser.
                cancel = true
            }
        }
        lock.unlock()
        if cancel {
            task.cancel()
            return
        }
        for event in events {
            lock.lock()
            let live = !stopped && task === self.task
            lock.unlock()
            if !live {
                break
            }
            RealtimeStream.applyEvent(event, invalidate: invalidate)
        }
    }

    fileprivate func complete(task: URLSessionTask, error: Error?) {
        var message: String?
        lock.lock()
        guard task === self.task else {
            lock.unlock()
            return
        }
        self.task = nil
        if stopped {
            lock.unlock()
            return
        }
        if !responseSeen && task.response != nil {
            _ = classifyLocked(task.response)
        }
        let end: StreamEnd
        if openedAt != nil {
            end = .dropped
        } else if refused {
            end = .refused
        } else {
            end = .unreachable
        }
        if end == .dropped, let opened = openedAt {
            let upMillis = (DispatchTime.now().uptimeNanoseconds &- opened) / 1_000_000
            if upMillis >= UInt64(max(timings.stableAfterMillis, 0)) {
                attempt = 0
            }
        }
        let wait = timings.nextDelay(
            end: end, status: refusedStatus, retryAfterMillis: retryAfterMillis, attempt: attempt
        )
        if attempt < RealtimeStream.maxAttempt {
            attempt += 1
        }
        if end == .dropped {
            failureLogged = false
        } else if !failureLogged {
            failureLogged = true
            message = RealtimeStream.failureMessage(end: end, status: refusedStatus, error: error)
                + "; retrying in \(wait) ms"
        }
        lock.unlock()
        if let message {
            log(message)
        }
        timerQueue.asyncAfter(deadline: .now() + .milliseconds(wait)) { [weak self] in
            self?.connect()
        }
    }

    // MARK: - Pure helpers

    /// `#if DEBUG` console output; release builds log nothing.
    static func debugLog(_ message: String) {
        #if DEBUG
        print("[OttoFlags] \(message)")
        #endif
    }

    /// A failure description without the key or the URL.
    static func failureMessage(end: StreamEnd, status: Int, error: Error?) -> String {
        switch end {
        case .unreachable:
            let cause: String
            if let urlError = error as? URLError {
                cause = "URLError \(urlError.code.rawValue)"
            } else if let error {
                cause = String(describing: type(of: error))
            } else {
                cause = "unknown error"
            }
            return "Otto Flags stream: connect failed: \(cause)"
        case .refused:
            if (200...299).contains(status) {
                return "Otto Flags stream: the response is not text/event-stream (status \(status))"
            }
            return "Otto Flags stream: connect refused (status \(status))"
        case .dropped:
            return "Otto Flags stream: the stream ended"
        }
    }

    /// Whether a Content-Type names `text/event-stream` (parameters and case ignored).
    static func isEventStream(_ contentType: String?) -> Bool {
        guard let contentType else {
            return false
        }
        let mediaType = contentType.components(separatedBy: ";").first ?? ""
        return mediaType.trimmingCharacters(in: .whitespaces).lowercased() == "text/event-stream"
    }

    /// `<apiUrl>/api/flags/stream`, keeping any path prefix in `apiUrl` (a trailing slash is
    /// absorbed) and dropping any query, fragment and user info; nil when `apiUrl` is not an
    /// http(s) URL with a host.
    static func streamURL(apiUrl: String) -> URL? {
        guard var components = URLComponents(string: apiUrl),
              let scheme = components.scheme?.lowercased(),
              scheme == "http" || scheme == "https",
              let host = components.host, !host.isEmpty else {
            return nil
        }
        var path = components.percentEncodedPath
        if path.hasSuffix("/") {
            path.removeLast()
        }
        components.percentEncodedPath = path + "/api/flags/stream"
        components.query = nil
        components.fragment = nil
        components.user = nil
        components.password = nil
        return components.url
    }

    /// Applies one event. `heartbeat` and events this SDK does not know are ignored. A flag event
    /// can never clear the whole cache: only `connected` passes nil.
    static func applyEvent(_ event: SSEEvent, invalidate: (String?) -> Void) {
        switch event.event {
        case "connected":
            invalidate(nil)
        case "flag.created", "flag.updated", "flag.deleted":
            if let key = changedFlagKey(event.data) {
                invalidate(key)
            }
        default:
            break
        }
    }

    /// The key of a flag event, if its data is a JSON object with a string `key` that a flag can
    /// have; else nil. Event text is untrusted: it is only ever used as a cache key, and only
    /// after `OttoFlagsClient.isValidFlagKey`.
    static func changedFlagKey(_ data: String) -> String? {
        guard let bytes = data.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: bytes, options: []),
              let dictionary = object as? [String: Any],
              let key = dictionary["key"] as? String,
              OttoFlagsClient.isValidFlagKey(key) else {
            return nil
        }
        return key
    }

    /// Maps `unit` into [0, 1]; NaN is 0.
    private static func clampUnit(_ unit: Double) -> Double {
        if unit.isNaN || unit < 0 {
            return 0
        }
        return min(unit, 1)
    }

    /// Converts a non-negative finite product to Int, saturating.
    private static func saturatedInt(_ value: Double) -> Int {
        if value.isNaN || value <= 0 {
            return 0
        }
        if value >= Double(Int.max) {
            return Int.max
        }
        return Int(value)
    }

    /// The wait before reconnect attempt `attempt` (from 0): `min(base * 2^attempt, ceiling)`,
    /// scaled into [50%, 100%] by `unit` in [0, 1), so clients dropped together do not reconnect
    /// together. A unit out of range is clamped.
    static func reconnectDelay(baseMillis: Int, attempt: Int, ceilingMillis: Int, unit: Double) -> Int {
        if baseMillis <= 0 || ceilingMillis <= 0 {
            return 0
        }
        var delay = baseMillis
        var index = 0
        while index < attempt && delay < ceilingMillis {
            delay = delay > Int.max / 2 ? Int.max : delay * 2
            index += 1
        }
        delay = min(delay, ceilingMillis)
        return saturatedInt(Double(delay) * (0.5 + clampUnit(unit) / 2))
    }

    /// The extra wait on top of `reconnectDelay` for the first retry after an established stream
    /// ended, uniform in [0, spread) by `unit`; zero otherwise.
    static func reconnectSpread(spreadMillis: Int, attempt: Int, wasUp: Bool, unit: Double) -> Int {
        if attempt != 0 || !wasUp || spreadMillis <= 0 || unit.isNaN {
            return 0
        }
        return saturatedInt(Double(spreadMillis) * clampUnit(unit))
    }

    /// A Retry-After header given in seconds, in milliseconds; -1 when absent or not a plain
    /// non-negative integer (an HTTP date, a negative or fractional value). A value too large
    /// for an Int saturates.
    static func parseRetryAfter(_ value: String?) -> Int {
        guard let trimmed = value?.trimmingCharacters(in: .whitespaces), !trimmed.isEmpty,
              trimmed.utf8.allSatisfy({ $0 >= 0x30 && $0 <= 0x39 }) else {
            return -1
        }
        guard let seconds = Int(trimmed) else {
            return Int.max
        }
        return seconds > Int.max / 1000 ? Int.max : seconds * 1000
    }

    static func saturatedAdd(_ a: Int, _ b: Int) -> Int {
        let (sum, overflow) = a.addingReportingOverflow(b)
        return overflow ? Int.max : sum
    }
}

/// The stream session's delegate. It holds the stream weakly: a `URLSession` retains its delegate
/// strongly until invalidated, and the stream owns the session. `stream` is written once in
/// `RealtimeStream.start()`, before the session exists, and only read afterwards.
final class RealtimeStreamDelegate: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    weak var stream: RealtimeStream?

    func urlSession(
        _ session: URLSession,
        dataTask: URLSessionDataTask,
        didReceive response: URLResponse,
        completionHandler: @escaping (URLSession.ResponseDisposition) -> Void
    ) {
        let allow = stream?.accept(response, task: dataTask) ?? false
        completionHandler(allow ? .allow : .cancel)
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        stream?.receive(data, task: dataTask)
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        stream?.complete(task: task, error: error)
    }

    /// A redirect could carry the key elsewhere: never follow it. The 3xx response is then
    /// delivered as the task's response, which the stream treats as a refused attempt.
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        completionHandler(nil)
    }
}
