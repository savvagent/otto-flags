import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// Configuration for the Otto Flags SDK client
public struct OttoFlagsConfig {
    let apiUrl: String
    let sdkKey: String
    let environment: String
    let pollingInterval: TimeInterval
    /// Ignored. The server has no WebSocket endpoint and the SDK no longer opens one
    /// (savvagent/otto-flags#125); kept so existing callers still compile.
    let enableWebSocket: Bool
    let timeout: TimeInterval

    public init(
        apiUrl: String = "https://otto-flags.savvagent.com",
        sdkKey: String,
        environment: String = "production",
        pollingInterval: TimeInterval = 60,
        enableWebSocket: Bool = true,
        timeout: TimeInterval = 30
    ) {
        self.apiUrl = apiUrl
        self.sdkKey = sdkKey
        self.environment = environment
        self.pollingInterval = pollingInterval
        self.enableWebSocket = enableWebSocket
        self.timeout = timeout
    }
}

/// User context for feature flag evaluation
public struct UserContext {
    let userId: String
    let attributes: [String: Any]

    public init(userId: String, attributes: [String: Any] = [:]) {
        self.userId = userId
        self.attributes = attributes
    }

    func toJSON() -> [String: Any] {
        return [
            "userId": userId,
            "attributes": attributes
        ]
    }
}

/// Result from flag evaluation
public struct FlagEvaluationResult {
    public let key: String
    public let value: Bool
    public let configuration: [String: Any]?
    public let variation: String?
    public let reason: String

    public init(key: String, value: Bool, configuration: [String: Any]? = nil, variation: String? = nil, reason: String = "evaluated") {
        self.key = key
        self.value = value
        self.configuration = configuration
        self.variation = variation
        self.reason = reason
    }
}

/// Result from variation evaluation for multi-variant flags
public struct VariationResult {
    public let variation: String
    public let enabled: Bool
    public let configuration: [String: Any]?

    public init(variation: String, enabled: Bool, configuration: [String: Any]? = nil) {
        self.variation = variation
        self.enabled = enabled
        self.configuration = configuration
    }
}

/// Cache entry for flag values
private struct CacheEntry {
    let value: Bool
    let configuration: [String: Any]?
    let variation: String?
}

/// Configuration override entry
private struct ConfigOverrideEntry {
    let config: [String: Any]
    let merge: Bool
    let timestamp: Date
}

/// Variation override entry
private struct VariationOverrideEntry {
    let variation: String
    let timestamp: Date
}

/// Main Otto Flags SDK client for feature flag evaluation
public class OttoFlagsClient {
    private let config: OttoFlagsConfig
    /// Evaluations by flag key, then by context key (the sorted-keys JSON of the request's
    /// `context` object): a flag's result depends on who is asking, so the flag key alone is
    /// not enough (savvagent/otto-flags#127). In memory only, bounded per flag.
    private var flagCache: [String: [String: CacheEntry]] = [:]
    private static let maxContextsPerFlag = 10_000
    private var configOverrides: [String: ConfigOverrideEntry] = [:]
    private var variationOverrides: [String: VariationOverrideEntry] = [:]
    private let session: URLSession
    private let queue = DispatchQueue(label: "com.savvagent.ottoflags.sdk", qos: .utility)

    private func configOverride(for flagKey: String) -> ConfigOverrideEntry? {
        queue.sync { configOverrides[flagKey] }
    }

    private func variationOverride(for flagKey: String) -> VariationOverrideEntry? {
        queue.sync { variationOverrides[flagKey] }
    }

    /// Initialize the Otto Flags client
    /// - Parameter config: Configuration for the SDK
    public convenience init(config: OttoFlagsConfig) {
        self.init(config: config, session: URLSession(configuration: .default))
    }

    /// Initialize the client with a caller-supplied URLSession (tests inject a mock protocol here)
    internal init(config: OttoFlagsConfig, session: URLSession) {
        self.config = config
        self.session = session

        startPolling()
    }

    /// Whether `key` is a flag key the SDK will put in a URL.
    ///
    /// Mirrors the server's key rule: 1 to 128 bytes, each one of `A-Z a-z 0-9 - _ .`,
    /// and not `.` or `..`. Anything else (`/`, `?`, `#`, `%`, spaces, non-ASCII) could
    /// steer the request, and the SDK key in its Authorization header, to a different
    /// endpoint, and `.` and `..` cannot be carried in a URL path at all because clients
    /// normalize them away (savvagent/otto-flags#107). Checked on UTF-8 bytes, so
    /// Unicode-wide character classes cannot slip through. Whole-string match, no trimming.
    private static func isValidFlagKey(_ key: String) -> Bool {
        let bytes = key.utf8
        guard (1...128).contains(bytes.count) else { return false }
        if key == "." || key == ".." { return false }
        return bytes.allSatisfy { b in
            (b >= 0x41 && b <= 0x5A) || (b >= 0x61 && b <= 0x7A) || (b >= 0x30 && b <= 0x39)
                || b == 0x2D || b == 0x5F || b == 0x2E
        }
    }

    /// Short description of a rejected key for the error message: the first 64
    /// Unicode scalars quoted and escaped, plus the full length in UTF-8 bytes (the
    /// unit the key rule counts).
    private static func describeInvalidKey(_ key: String) -> String {
        "\(String(reflecting: String(String.UnicodeScalarView(key.unicodeScalars.prefix(64))))) (\(key.utf8.count) bytes)"
    }

    /// RFC 3986 unreserved characters only (not `.alphanumerics`, which is Unicode-wide).
    private static let pathSegmentAllowed = CharacterSet(
        charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~"
    )

    private static func encodePathSegment(_ key: String) -> String? {
        key.addingPercentEncoding(withAllowedCharacters: pathSegmentAllowed)
    }

    /// Evaluate a feature flag and get full details (Phase 1 & 2)
    /// - Parameters:
    ///   - flagKey: The key of the feature flag
    ///   - context: User context for evaluation
    /// - Returns: FlagEvaluationResult with value, configuration, and variation
    public func evaluate(flagKey: String, context: UserContext) async throws -> FlagEvaluationResult {
        guard Self.isValidFlagKey(flagKey) else {
            throw OttoFlagsError.invalidFlagKey(Self.describeInvalidKey(flagKey))
        }

        // JSONSerialization raises an uncatchable exception for a value JSON cannot hold
        // (a Date, a custom type in the attributes); check first and fail the lookup instead.
        let evaluateBody = evaluationBody(for: context)
        guard JSONSerialization.isValidJSONObject(evaluateBody) else {
            throw OttoFlagsError.invalidContext
        }

        // The cache key is the request's context, computed before the lookup.
        let contextKey: String? = evaluateBody["context"]
            .flatMap { try? JSONSerialization.data(withJSONObject: $0, options: [.sortedKeys]) }
            .flatMap { String(data: $0, encoding: .utf8) }

        // Check cache first
        let cachedEntry: CacheEntry? = contextKey.flatMap { key in
            queue.sync { flagCache[flagKey]?[key] }
        }
        if let cached = cachedEntry {
            var configuration = cached.configuration
            var variation = cached.variation

            // Apply overrides
            if let configOverride = configOverride(for: flagKey) {
                configuration = if configOverride.merge, let baseConfig = configuration {
                    mergeConfigurations(base: baseConfig, override: configOverride.config)
                } else {
                    configOverride.config
                }
            }

            if let variationOverride = variationOverride(for: flagKey) {
                variation = variationOverride.variation
            }

            return FlagEvaluationResult(
                key: flagKey,
                value: cached.value,
                configuration: configuration,
                variation: variation,
                reason: "cached"
            )
        }

        // Fetch from API
        guard let encodedKey = Self.encodePathSegment(flagKey),
              let url = URL(string: "\(config.apiUrl)/api/flags/\(encodedKey)/evaluate") else {
            throw OttoFlagsError.invalidURL
        }

        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("Bearer \(config.sdkKey)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.timeoutInterval = config.timeout

        request.httpBody = try JSONSerialization.data(withJSONObject: evaluateBody)

        let (data, response) = try await session.fetchData(for: request)

        guard let httpResponse = response as? HTTPURLResponse,
              (200...299).contains(httpResponse.statusCode) else {
            throw OttoFlagsError.requestFailed
        }

        guard let json = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let enabled = json["enabled"] as? Bool else {
            throw OttoFlagsError.invalidResponse
        }

        var configuration = json["configuration"] as? [String: Any]
        var variation = json["variation"] as? String

        // Update cache
        if let contextKey {
            let entry = CacheEntry(value: enabled, configuration: configuration, variation: variation)
            queue.sync {
                // Mutate in place: copying the inner dictionary out and back would copy up to
                // maxContextsPerFlag entries on every miss.
                if (flagCache[flagKey]?.count ?? 0) >= Self.maxContextsPerFlag
                    && flagCache[flagKey]?[contextKey] == nil {
                    flagCache[flagKey] = [:]
                }
                flagCache[flagKey, default: [:]][contextKey] = entry
            }
        }

        // Apply overrides to evaluated result
        if let configOverride = configOverride(for: flagKey) {
            configuration = if configOverride.merge, let baseConfig = configuration {
                mergeConfigurations(base: baseConfig, override: configOverride.config)
            } else {
                configOverride.config
            }
        }

        if let variationOverride = variationOverride(for: flagKey) {
            variation = variationOverride.variation
        }

        return FlagEvaluationResult(
            key: flagKey,
            value: enabled,
            configuration: configuration,
            variation: variation,
            reason: "evaluated"
        )
    }

    /// Check if a feature flag is enabled for a given user context (convenience method)
    /// - Parameters:
    ///   - flagKey: The key of the feature flag
    ///   - context: User context for evaluation
    /// - Returns: Boolean indicating if the flag is enabled
    public func isEnabled(flagKey: String, context: UserContext) async throws -> Bool {
        let result = try await evaluate(flagKey: flagKey, context: context)
        return result.value
    }

    /// Get dynamic configuration for a flag (Phase 1)
    /// - Parameters:
    ///   - flagKey: The key of the feature flag
    ///   - context: User context for evaluation
    /// - Returns: Configuration dictionary if flag is enabled, otherwise nil
    public func getConfig(flagKey: String, context: UserContext) async throws -> [String: Any]? {
        let result = try await evaluate(flagKey: flagKey, context: context)
        return result.value ? result.configuration : nil
    }

    /// Get variation details for multi-variant flags (Phase 2)
    /// - Parameters:
    ///   - flagKey: The key of the feature flag
    ///   - context: User context for evaluation
    /// - Returns: VariationResult with variation name, enabled status, and configuration
    public func getVariationDetails(flagKey: String, context: UserContext) async throws -> VariationResult {
        let result = try await evaluate(flagKey: flagKey, context: context)
        return VariationResult(
            variation: result.variation ?? "control",
            enabled: result.value,
            configuration: result.configuration
        )
    }

    /// Get a variation value for a feature flag (legacy method - kept for backward compatibility)
    ///
    /// The server has no variation endpoint: this evaluates the flag through `evaluate`, so the
    /// cache and local variation overrides apply. It returns
    /// - the variation name when `defaultValue` is a `String` (or `defaultValue` when the flag
    ///   has no variation),
    /// - whether the flag is enabled when `defaultValue` is a `Bool`,
    /// - `defaultValue` for any other type, and on any failure.
    /// - Parameters:
    ///   - flagKey: The key of the feature flag
    ///   - context: User context for evaluation
    ///   - defaultValue: Default value if flag evaluation fails
    /// - Returns: The variation value or default
    public func getVariation<T>(flagKey: String, context: UserContext, defaultValue: T) async -> T {
        guard let result = try? await evaluate(flagKey: flagKey, context: context) else {
            return defaultValue
        }
        if defaultValue is String {
            // Unwrap first: with an optional T, `nil as? T` would succeed as a wrapped nil.
            guard let name = result.variation, let value = name as? T else {
                return defaultValue
            }
            return value
        }
        if defaultValue is Bool {
            return (result.value as? T) ?? defaultValue
        }
        return defaultValue
    }

    /// The evaluate request body the SDK guide documents: `{"context": {...}}` with the
    /// server's snake_case context fields. `UserContext.toJSON()` is the `track` event shape,
    /// which the server does not read here.
    func evaluationBody(for context: UserContext) -> [String: Any] {
        [
            "context": [
                "user_id": context.userId,
                "environment": config.environment,
                "attributes": context.attributes,
            ] as [String: Any],
        ]
    }

    /// Track an event for analytics.
    ///
    /// A documented no-op (savvagent/otto-flags#125): the server has no custom-event endpoint
    /// (`docs/SDK-DEVELOPER-GUIDE.md` lists only the evaluation and error telemetry routes), so
    /// nothing is sent. The method stays so existing callers compile.
    /// - Parameters:
    ///   - eventName: Name of the event
    ///   - context: User context
    ///   - properties: Additional event properties
    public func track(eventName: String, context: UserContext, properties: [String: Any] = [:]) async {
    }

    /// Flush any pending events or data
    public func flush() async {
        // Implementation for flushing pending events
    }

    /// Close the client and cleanup resources
    public func close() {
    }

    /// Set a configuration override for a flag
    /// Useful for testing different configuration values without server changes
    /// - Parameters:
    ///   - flagKey: The key of the feature flag
    ///   - config: Configuration dictionary to override with
    ///   - merge: Whether to merge with existing configuration (default: false)
    public func setConfigOverride(flagKey: String, config: [String: Any], merge: Bool = false) {
        queue.sync {
            self.configOverrides[flagKey] = ConfigOverrideEntry(config: config, merge: merge, timestamp: Date())
            // Clear cache to force re-evaluation with override
            self.flagCache.removeValue(forKey: flagKey)
        }
    }

    /// Clear configuration override for a flag
    /// - Parameter flagKey: The key of the feature flag
    public func clearConfigOverride(flagKey: String) {
        queue.sync {
            self.configOverrides.removeValue(forKey: flagKey)
            // Clear cache to get fresh API values
            self.flagCache.removeValue(forKey: flagKey)
        }
    }

    /// Set a variation override for a multi-variant flag
    /// Forces the flag to return a specific variation
    /// - Parameters:
    ///   - flagKey: The key of the feature flag
    ///   - variation: The variation identifier to force
    public func setVariationOverride(flagKey: String, variation: String) {
        queue.sync {
            self.variationOverrides[flagKey] = VariationOverrideEntry(variation: variation, timestamp: Date())
            // Clear cache to force re-evaluation with override
            self.flagCache.removeValue(forKey: flagKey)
        }
    }

    /// Clear variation override for a flag
    /// - Parameter flagKey: The key of the feature flag
    public func clearVariationOverride(flagKey: String) {
        queue.sync {
            self.variationOverrides.removeValue(forKey: flagKey)
            // Clear cache to get fresh API values
            self.flagCache.removeValue(forKey: flagKey)
        }
    }

    /// Check if a flag has a configuration override
    /// - Parameter flagKey: The key of the feature flag
    /// - Returns: Boolean indicating if override exists
    public func hasConfigOverride(flagKey: String) -> Bool {
        return queue.sync { configOverrides[flagKey] != nil }
    }

    /// Check if a flag has a variation override
    /// - Parameter flagKey: The key of the feature flag
    /// - Returns: Boolean indicating if override exists
    public func hasVariationOverride(flagKey: String) -> Bool {
        return queue.sync { variationOverrides[flagKey] != nil }
    }

    /// Get all configuration overrides (for debugging/inspection)
    /// - Returns: Dictionary mapping flag keys to override details
    public func getConfigOverrides() -> [String: [String: Any]] {
        return queue.sync {
            configOverrides.mapValues { entry in
                [
                    "config": entry.config,
                    "merge": entry.merge,
                    "timestamp": entry.timestamp
                ] as [String: Any]
            }
        }
    }

    /// Get all variation overrides (for debugging/inspection)
    /// - Returns: Dictionary mapping flag keys to override details
    public func getVariationOverrides() -> [String: [String: Any]] {
        return queue.sync {
            variationOverrides.mapValues { entry in
                [
                    "variation": entry.variation,
                    "timestamp": entry.timestamp
                ] as [String: Any]
            }
        }
    }

    /// Clear all configuration and variation overrides
    public func clearAllOverrides() {
        queue.sync {
            self.configOverrides.removeAll()
            self.variationOverrides.removeAll()
            self.flagCache.removeAll()
        }
    }

    // MARK: - Private Methods

    private func startPolling() {
        guard config.pollingInterval > 0 else { return }

        queue.asyncAfter(deadline: .now() + config.pollingInterval) { [weak self] in
            Task { await self?.refreshFlags() }
            self?.startPolling()
        }
    }

    /// Poll `GET /api/sdk/flags?environment=<env>` (docs/SDK-DEVELOPER-GUIDE.md) and drop the
    /// cached results of every flag it lists, so the next lookup re-evaluates. Never throws:
    /// a failed request, a non-2xx status or an unparseable body leaves the cache alone.
    func refreshFlags() async {
        guard var components = URLComponents(string: "\(config.apiUrl)/api/sdk/flags") else {
            return
        }
        components.queryItems = [URLQueryItem(name: "environment", value: config.environment)]
        guard let url = components.url else {
            return
        }

        var request = URLRequest(url: url)
        request.httpMethod = "GET"
        request.setValue("Bearer \(config.sdkKey)", forHTTPHeaderField: "Authorization")
        request.timeoutInterval = config.timeout

        do {
            let (data, response) = try await session.fetchData(for: request)
            guard let httpResponse = response as? HTTPURLResponse,
                  (200...299).contains(httpResponse.statusCode),
                  let json = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let flags = json["flags"] as? [[String: Any]] else {
                return
            }

            for flag in flags {
                if let key = flag["key"] as? String {
                    queue.async { [weak self] in
                        self?.flagCache.removeValue(forKey: key)
                    }
                }
            }
        } catch {
            // Silently fail for polling
        }
    }

    /// Merge two configuration dictionaries (for partial overrides)
    /// Deep merge where override values take precedence
    /// - Parameters:
    ///   - base: Base configuration dictionary
    ///   - override: Override configuration dictionary
    /// - Returns: Merged configuration dictionary
    private func mergeConfigurations(base: [String: Any], override: [String: Any]) -> [String: Any] {
        var result = base

        for (key, overrideValue) in override {
            if let baseValue = result[key] as? [String: Any],
               let overrideDict = overrideValue as? [String: Any] {
                // Recursively merge nested dictionaries
                result[key] = mergeConfigurations(base: baseValue, override: overrideDict)
            } else {
                // Override the value
                result[key] = overrideValue
            }
        }

        return result
    }
}

/// Errors that can occur in the Otto Flags SDK
public enum OttoFlagsError: Error {
    case invalidURL
    /// The flag key is not 1-128 characters of `A-Z a-z 0-9 - _ .` (or is `.` or `..`);
    /// no request was made. The associated text describes the rejected key.
    case invalidFlagKey(String)
    case requestFailed
    case invalidResponse
    case networkError(Error)
    /// The context's attributes hold a value JSON cannot represent; no request was made.
    case invalidContext
}
