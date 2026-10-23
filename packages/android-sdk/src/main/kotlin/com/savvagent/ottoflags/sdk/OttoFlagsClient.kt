package com.savvagent.ottoflags.sdk

import android.content.Context
import android.util.Log
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import okhttp3.*
import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit

/**
 * Configuration for the Otto Flags SDK client
 */
data class OttoFlagsConfig(
    val apiUrl: String = "https://otto-flags.savvagent.com",
    val sdkKey: String,
    val environment: String = "production",
    val pollingInterval: Long = 60_000L, // milliseconds
    /** Ignored: the server has no WebSocket endpoint (savvagent/otto-flags#125); kept so callers compile. */
    val enableWebSocket: Boolean = true,
    val timeout: Long = 30_000L, // milliseconds
    val enableLogging: Boolean = false
)

/**
 * User context for feature flag evaluation
 */
data class UserContext(
    val userId: String,
    val attributes: Map<String, Any> = emptyMap()
) {
    fun toJson(): JSONObject {
        return JSONObject().apply {
            put("userId", userId)
            put("attributes", JSONObject(attributes))
        }
    }
}

/**
 * Result from flag evaluation
 */
data class FlagEvaluationResult(
    val key: String,
    val value: Boolean,
    val configuration: Map<String, Any>? = null,
    val variation: String? = null,
    val reason: String = "evaluated"
)

/**
 * Result from variation evaluation for multi-variant flags
 */
data class VariationResult(
    val variation: String,
    val enabled: Boolean,
    val configuration: Map<String, Any>? = null
)

/**
 * Cache entry for flag values
 */
private data class CacheEntry(
    val value: Boolean,
    val configuration: Map<String, Any>? = null,
    val variation: String? = null
)

/**
 * Configuration override entry
 */
private data class ConfigOverrideEntry(
    val config: Map<String, Any>,
    val merge: Boolean,
    val timestamp: Long
)

/**
 * Variation override entry
 */
private data class VariationOverrideEntry(
    val variation: String,
    val timestamp: Long
)

/**
 * Main Otto Flags SDK client for feature flag evaluation
 */
class OttoFlagsClient(
    private val config: OttoFlagsConfig,
    private val context: Context? = null
) {
    private val okHttpClient: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(config.timeout, TimeUnit.MILLISECONDS)
        .readTimeout(config.timeout, TimeUnit.MILLISECONDS)
        .writeTimeout(config.timeout, TimeUnit.MILLISECONDS)
        .apply {
            if (config.enableLogging) {
                addInterceptor(okhttp3.logging.HttpLoggingInterceptor().apply {
                    level = okhttp3.logging.HttpLoggingInterceptor.Level.BODY
                })
            }
        }
        .build()

    // flag key -> (context key -> entry): a result is only reused for the context that
    // produced it. In memory only; bounded per flag by MAX_CONTEXTS_PER_FLAG.
    private val flagCache = ConcurrentHashMap<String, ConcurrentHashMap<String, CacheEntry>>()
    private val configOverrides = ConcurrentHashMap<String, ConfigOverrideEntry>()
    private val variationOverrides = ConcurrentHashMap<String, VariationOverrideEntry>()
    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private var pollingJob: Job? = null

    private val _flagUpdates = MutableStateFlow<Map<String, Boolean>>(emptyMap())
    val flagUpdates: StateFlow<Map<String, Boolean>> = _flagUpdates

    init {
        startPolling()
    }

    /**
     * Evaluate a feature flag and get full details (Phase 1 & 2)
     */
    suspend fun evaluate(flagKey: String, userContext: UserContext): Result<FlagEvaluationResult> {
        return withContext(Dispatchers.IO) {
            try {
                if (!isValidFlagKey(flagKey)) {
                    val message = invalidKeyMessage(flagKey)
                    logError(message)
                    return@withContext Result.failure(OttoFlagsException(message))
                }

                // The context key is computed before the lookup: the cache is per context.
                val json = evaluationBody(userContext)
                val ctxKey = contextKey(json)

                // Check cache first
                flagCache[flagKey]?.get(ctxKey)?.let { cached ->
                    var configuration = cached.configuration
                    var variation = cached.variation

                    // Apply overrides
                    configOverrides[flagKey]?.let { override ->
                        configuration = if (override.merge && configuration != null) {
                            mergeConfigurations(configuration, override.config)
                        } else {
                            override.config
                        }
                    }

                    variationOverrides[flagKey]?.let { override ->
                        variation = override.variation
                    }

                    return@withContext Result.success(
                        FlagEvaluationResult(
                            key = flagKey,
                            value = cached.value,
                            configuration = configuration,
                            variation = variation,
                            reason = "cached"
                        )
                    )
                }

                // Fetch from API
                val url = evaluateUrl(flagKey)

                val request = Request.Builder()
                    .url(url)
                    .post(json.toString().toRequestBody("application/json".toMediaType()))
                    .addHeader("Authorization", "Bearer ${config.sdkKey}")
                    .build()

                val response = okHttpClient.newCall(request).execute()

                if (!response.isSuccessful) {
                    val message = "Request failed with code: ${response.code}"
                    logError("Error checking flag: $flagKey: $message")
                    return@withContext Result.failure(OttoFlagsException(message))
                }

                val body = response.body?.string() ?: ""
                val jsonResponse = JSONObject(body)
                // The result is "enabled" (docs/SDK-DEVELOPER-GUIDE.md). getBoolean would
                // also accept the strings "true"/"false"; only a JSON boolean counts (#112).
                val enabled = jsonResponse.opt("enabled") as? Boolean
                    ?: run {
                        val message = "The evaluate response for $flagKey has no boolean \"enabled\""
                        logError(message)
                        return@withContext Result.failure(OttoFlagsException(message))
                    }

                // The server sends both fields, as null when the flag has none: isNull is
                // true for a missing key and for JSON null.
                var configuration = if (jsonResponse.isNull("configuration")) {
                    null
                } else {
                    jsonResponse.getJSONObject("configuration").toMap()
                }

                // opt returns JSONObject.NULL for JSON null, which is not a String; getString
                // would also turn a number into a string.
                var variation = jsonResponse.opt("variation") as? String

                // Update cache
                val perFlag = flagCache.getOrPut(flagKey) { ConcurrentHashMap() }
                synchronized(perFlag) {
                    if (perFlag.size >= MAX_CONTEXTS_PER_FLAG && !perFlag.containsKey(ctxKey)) {
                        perFlag.clear()
                    }
                    perFlag[ctxKey] = CacheEntry(enabled, configuration, variation)
                }
                _flagUpdates.update { it + (flagKey to enabled) }

                // Apply overrides to evaluated result
                configOverrides[flagKey]?.let { override ->
                    configuration = if (override.merge && configuration != null) {
                        mergeConfigurations(configuration, override.config)
                    } else {
                        override.config
                    }
                }

                variationOverrides[flagKey]?.let { override ->
                    variation = override.variation
                }

                Result.success(FlagEvaluationResult(
                    key = flagKey,
                    value = enabled,
                    configuration = configuration,
                    variation = variation,
                    reason = "evaluated"
                ))
            } catch (e: Exception) {
                logError("Error checking flag: $flagKey", e)
                Result.failure(e)
            }
        }
    }

    /**
     * Check if a feature flag is enabled for a given user context (convenience method)
     */
    suspend fun isEnabled(flagKey: String, userContext: UserContext): Result<Boolean> {
        return evaluate(flagKey, userContext).map { it.value }
    }

    /**
     * Get dynamic configuration for a flag (Phase 1)
     * Returns configuration if flag is enabled, otherwise returns null
     */
    suspend fun getConfig(flagKey: String, userContext: UserContext): Result<Map<String, Any>?> {
        return evaluate(flagKey, userContext).map { result ->
            if (result.value) result.configuration else null
        }
    }

    /**
     * Get variation details for multi-variant flags (Phase 2)
     * Returns variation name, enabled status, and configuration
     */
    suspend fun getVariationDetails(flagKey: String, userContext: UserContext): Result<VariationResult> {
        return evaluate(flagKey, userContext).map { result ->
            VariationResult(
                variation = result.variation ?: "control",
                enabled = result.value,
                configuration = result.configuration
            )
        }
    }

    /**
     * Get a variation value for a feature flag (legacy method - kept for backward compatibility)
     *
     * The server has no variation endpoint: this evaluates the flag through [evaluate], so the
     * cache and local variation overrides apply. It returns
     * - the variation name when [defaultValue] is a `String` (or [defaultValue] when the flag
     *   has no variation),
     * - whether the flag is enabled when [defaultValue] is a `Boolean`,
     * - [defaultValue] for any other type, and on any failure.
     */
    @Suppress("UNCHECKED_CAST")
    suspend fun <T> getVariation(
        flagKey: String,
        userContext: UserContext,
        defaultValue: T
    ): T {
        val result = evaluate(flagKey, userContext).getOrNull() ?: return defaultValue
        return when (defaultValue) {
            is String -> (result.variation as T?) ?: defaultValue
            is Boolean -> result.value as T
            else -> defaultValue
        }
    }

    /**
     * Track an event for analytics.
     *
     * A documented no-op (savvagent/otto-flags#125): the server has no custom-event endpoint
     * (docs/SDK-DEVELOPER-GUIDE.md lists only the evaluation and error telemetry routes), so
     * nothing is sent. Kept so existing callers compile.
     */
    @Suppress("UNUSED_PARAMETER")
    suspend fun track(
        eventName: String,
        userContext: UserContext,
        properties: Map<String, Any> = emptyMap()
    ) {
    }

    /**
     * Flush any pending events or data
     */
    suspend fun flush() {
        // Implementation for flushing pending events
    }

    /**
     * Close the client and cleanup resources
     */
    fun close() {
        pollingJob?.cancel()
        scope.cancel()
    }

    /**
     * Set a configuration override for a flag
     * Useful for testing different configuration values without server changes
     */
    fun setConfigOverride(flagKey: String, config: Map<String, Any>, merge: Boolean = false) {
        configOverrides[flagKey] = ConfigOverrideEntry(config, merge, System.currentTimeMillis())
        // Clear cache to force re-evaluation with override
        flagCache.remove(flagKey)
    }

    /**
     * Clear configuration override for a flag
     */
    fun clearConfigOverride(flagKey: String) {
        configOverrides.remove(flagKey)
        // Clear cache to get fresh API values
        flagCache.remove(flagKey)
    }

    /**
     * Set a variation override for a multi-variant flag
     * Forces the flag to return a specific variation
     */
    fun setVariationOverride(flagKey: String, variation: String) {
        variationOverrides[flagKey] = VariationOverrideEntry(variation, System.currentTimeMillis())
        // Clear cache to force re-evaluation with override
        flagCache.remove(flagKey)
    }

    /**
     * Clear variation override for a flag
     */
    fun clearVariationOverride(flagKey: String) {
        variationOverrides.remove(flagKey)
        // Clear cache to get fresh API values
        flagCache.remove(flagKey)
    }

    /**
     * Check if a flag has a configuration override
     */
    fun hasConfigOverride(flagKey: String): Boolean {
        return configOverrides.containsKey(flagKey)
    }

    /**
     * Check if a flag has a variation override
     */
    fun hasVariationOverride(flagKey: String): Boolean {
        return variationOverrides.containsKey(flagKey)
    }

    /**
     * Get all configuration overrides (for debugging/inspection)
     */
    fun getConfigOverrides(): Map<String, Map<String, Any>> {
        return configOverrides.mapValues { (_, entry) ->
            mapOf(
                "config" to entry.config,
                "merge" to entry.merge,
                "timestamp" to entry.timestamp
            )
        }
    }

    /**
     * Get all variation overrides (for debugging/inspection)
     */
    fun getVariationOverrides(): Map<String, Map<String, Any>> {
        return variationOverrides.mapValues { (_, entry) ->
            mapOf(
                "variation" to entry.variation,
                "timestamp" to entry.timestamp
            )
        }
    }

    /**
     * Clear all configuration and variation overrides
     */
    fun clearAllOverrides() {
        configOverrides.clear()
        variationOverrides.clear()
        flagCache.clear()
    }

    // MARK: - Private Methods

    private fun startPolling() {
        if (config.pollingInterval <= 0) return

        pollingJob = scope.launch {
            while (isActive) {
                delay(config.pollingInterval)
                refreshFlags()
            }
        }
    }

    /**
     * Poll `GET /api/sdk/flags?environment=<env>` (docs/SDK-DEVELOPER-GUIDE.md) and drop the
     * cached results of every flag it lists, so the next lookup re-evaluates. Never throws:
     * a failed request, a non-2xx status or an unparseable body leaves the cache alone.
     */
    internal suspend fun refreshFlags() {
        try {
            val url = "${config.apiUrl}/api/sdk/flags".toHttpUrl().newBuilder()
                .addQueryParameter("environment", config.environment)
                .build()
            val request = Request.Builder()
                .url(url)
                .get()
                .addHeader("Authorization", "Bearer ${config.sdkKey}")
                .build()

            withContext(Dispatchers.IO) {
                okHttpClient.newCall(request).execute().use { response ->
                    if (response.isSuccessful) {
                        val body = response.body?.string() ?: ""
                        val flags = JSONObject(body).getJSONArray("flags")

                        for (i in 0 until flags.length()) {
                            val key = flags.getJSONObject(i).getString("key")
                            // The list carries no context; invalidate instead of writing.
                            flagCache.remove(key)
                        }
                    }
                }
            }
        } catch (e: Exception) {
            logError("Error refreshing flags", e)
        }
    }

    private fun logError(message: String, throwable: Throwable? = null) {
        if (config.enableLogging) {
            Log.e(TAG, message, throwable)
        }
    }

    /**
     * Merge two configuration maps (for partial overrides)
     * Deep merge where override values take precedence
     */
    @Suppress("UNCHECKED_CAST")
    private fun mergeConfigurations(base: Map<String, Any>, override: Map<String, Any>): Map<String, Any> {
        val result = base.toMutableMap()

        for ((key, overrideValue) in override) {
            val baseValue = result[key]

            if (baseValue is Map<*, *> && overrideValue is Map<*, *>) {
                // Recursively merge nested maps
                result[key] = mergeConfigurations(
                    baseValue as Map<String, Any>,
                    overrideValue as Map<String, Any>
                )
            } else {
                // Override the value
                result[key] = overrideValue
            }
        }

        return result
    }

    /**
     * Build `<apiUrl>/api/flags/<flagKey>/evaluate` (docs/SDK-DEVELOPER-GUIDE.md) with the
     * key as a single, encoded path segment. An unparsable `apiUrl` throws
     * IllegalArgumentException, which the caller's try block turns into its failure path.
     */
    internal fun evaluateUrl(flagKey: String): HttpUrl =
        config.apiUrl.toHttpUrl().newBuilder()
            .addPathSegment("api")
            .addPathSegment("flags")
            .addPathSegment(flagKey)
            .addPathSegment("evaluate")
            .build()

    /**
     * The evaluate request body the SDK guide documents: `{"context": {...}}` with the
     * server's snake_case context fields. [UserContext.toJson] is the `track` event shape,
     * which the server does not read here.
     */
    internal fun evaluationBody(userContext: UserContext): JSONObject =
        JSONObject().apply {
            put("context", JSONObject().apply {
                put("user_id", userContext.userId)
                put("environment", config.environment)
                put("attributes", JSONObject(userContext.attributes))
            })
        }

    /**
     * Cache key for the `context` object of an evaluate body: the same context renders
     * the same string whatever order its attributes were inserted in.
     */
    internal fun contextKey(body: JSONObject): String = canonicalJson(body.get("context"))

    private fun canonicalJson(value: Any?): String = when (value) {
        null, JSONObject.NULL -> "null"
        is JSONObject -> value.keys().asSequence().toList().sorted().joinToString(",", "{", "}") { k ->
            JSONObject.quote(k) + ":" + canonicalJson(value.opt(k))
        }
        is JSONArray -> (0 until value.length()).joinToString(",", "[", "]") { i ->
            canonicalJson(value.opt(i))
        }
        // Only numbers and booleans are bare in the request body; anything else (a String, a
        // Char) is sent quoted, so it is quoted here too and cannot share a key with a number.
        is Number, is Boolean -> value.toString()
        else -> JSONObject.quote(value.toString())
    }

    private fun invalidKeyMessage(flagKey: String): String =
        "Invalid flag key ${escapeForMessage(flagKey.take(64))} (${flagKey.length} characters); not evaluated"

    companion object {
        private const val TAG = "OttoFlagsClient"
        private const val MAX_FLAG_KEY_LENGTH = 128

        /** Most distinct contexts cached per flag; a full flag map is cleared first. */
        internal const val MAX_CONTEXTS_PER_FLAG = 10_000

        /**
         * Whether [key] is a flag key this SDK will put in a request URL: 1 to 128
         * characters, each one of A-Z a-z 0-9 `-` `_` `.`, and not `.` or `..`.
         *
         * This mirrors the server's key rule. A key outside it could steer the request
         * (and the SDK key in its Authorization header) to another endpoint: `/`, `?`
         * and `#` change the path or query, and `.` or `..` cannot be carried as a URL
         * path segment because clients normalize them away (#107). Whole-string match,
         * no trimming.
         */
        internal fun isValidFlagKey(key: String): Boolean {
            if (key.isEmpty() || key.length > MAX_FLAG_KEY_LENGTH) return false
            if (key == "." || key == "..") return false
            return key.all { c ->
                c in 'A'..'Z' || c in 'a'..'z' || c in '0'..'9' || c == '-' || c == '_' || c == '.'
            }
        }

        /**
         * Quote [text] for a log or error message: printable ASCII other than `"` and
         * `\` is kept, everything else becomes `\uXXXX`. Deliberately not org.json,
         * which is a stub in local unit tests.
         */
        internal fun escapeForMessage(text: String): String {
            val sb = StringBuilder("\"")
            for (c in text) {
                if (c in ' '..'~' && c != '"' && c != '\\') {
                    sb.append(c)
                } else {
                    sb.append("\\u").append(String.format("%04x", c.code))
                }
            }
            return sb.append('"').toString()
        }
    }
}

/**
 * Exception thrown by the Otto Flags SDK
 */
class OttoFlagsException(message: String, cause: Throwable? = null) : Exception(message, cause)

/**
 * Extension function to convert JSONObject to Map
 */
private fun JSONObject.toMap(): Map<String, Any> {
    val map = mutableMapOf<String, Any>()
    keys().forEach { key ->
        val value = get(key)
        map[key] = when (value) {
            is JSONObject -> value.toMap()
            is org.json.JSONArray -> {
                val list = mutableListOf<Any>()
                for (i in 0 until value.length()) {
                    list.add(value.get(i))
                }
                list
            }
            else -> value
        }
    }
    return map
}
