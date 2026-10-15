package com.savvagent.ottoflags.sdk;

import com.google.gson.Gson;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import okhttp3.*;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.io.IOException;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.TimeUnit;

/**
 * Otto Flags FlagClient for server-side feature flag evaluation
 */
public class FlagClient implements AutoCloseable {
    private static final Logger logger = LoggerFactory.getLogger(FlagClient.class);
    private static final MediaType JSON = MediaType.get("application/json; charset=utf-8");

    private final FlagClientConfig config;
    private final OkHttpClient httpClient;
    private final Gson gson;
    private final ConcurrentHashMap<String, CacheEntry> cache;
    private final ConcurrentHashMap<String, ConfigOverrideEntry> configOverrides;
    private final ConcurrentHashMap<String, VariationOverrideEntry> variationOverrides;

    public FlagClient(FlagClientConfig config) {
        this.config = config;
        this.gson = new Gson();
        this.cache = new ConcurrentHashMap<>();
        this.configOverrides = new ConcurrentHashMap<>();
        this.variationOverrides = new ConcurrentHashMap<>();

        this.httpClient = new OkHttpClient.Builder()
                .connectTimeout(config.getTimeout(), TimeUnit.MILLISECONDS)
                .readTimeout(config.getTimeout(), TimeUnit.MILLISECONDS)
                .writeTimeout(config.getTimeout(), TimeUnit.MILLISECONDS)
                .build();

        logger.info("Otto Flags FlagClient initialized");
    }

    /**
     * Evaluate a feature flag
     */
    public FlagEvaluationResult evaluate(String flagKey, FlagContext context) {
        long startTime = System.currentTimeMillis();

        if (!isValidFlagKey(flagKey)) {
            String message = describeInvalidFlagKey(flagKey);
            logger.error("{}", message);
            try {
                config.getOnError().accept(new IllegalArgumentException(message));
            } catch (RuntimeException callbackFailure) {
                logger.error("onError callback failed: {}", callbackFailure.getMessage());
            }
            return new FlagEvaluationResult(flagKey, defaultFor(flagKey), "error", null);
        }

        try {
            // Check cache first
            CacheEntry cachedEntry = getCachedEntry(flagKey);
            if (cachedEntry != null) {
                Object configuration = cachedEntry.configuration;
                String variation = cachedEntry.variation;

                // Apply overrides
                ConfigOverrideEntry configOverride = configOverrides.get(flagKey);
                if (configOverride != null) {
                    if (configOverride.merge && configuration != null) {
                        configuration = mergeConfigurations(configuration, configOverride.config);
                    } else {
                        configuration = configOverride.config;
                    }
                }

                VariationOverrideEntry variationOverride = variationOverrides.get(flagKey);
                if (variationOverride != null) {
                    variation = variationOverride.variation;
                }

                logger.debug("Flag '{}' returned from cache: {}", flagKey, cachedEntry.value);
                return new FlagEvaluationResult(flagKey, cachedEntry.value, configuration,
                        variation, "cached", null);
            }

            // Prepare context
            if (context == null) {
                context = new FlagContext();
            }
            if (context.getApplicationId() == null && !config.getApplicationId().isEmpty()) {
                context.setApplicationId(config.getApplicationId());
            }

            // Call API
            // Build the URL segment by segment so the key is encoded as one path segment.
            HttpUrl url = HttpUrl.get(config.getBaseUrl()).newBuilder()
                    .addPathSegment("api")
                    .addPathSegment("flags")
                    .addPathSegment(flagKey)
                    .addPathSegment("evaluate")
                    .build();
            JsonObject requestBody = new JsonObject();
            requestBody.add("context", gson.toJsonTree(context));

            Request request = new Request.Builder()
                    .url(url)
                    .post(RequestBody.create(gson.toJson(requestBody), JSON))
                    .addHeader("Authorization", "Bearer " + config.getApiKey())
                    .build();

            try (Response response = httpClient.newCall(request).execute()) {
                if (!response.isSuccessful()) {
                    throw new IOException("API error: " + response.code() + " " + response.message());
                }

                JsonObject data = gson.fromJson(response.body().string(), JsonObject.class);
                // The result is "enabled" (docs/SDK-DEVELOPER-GUIDE.md). A body without a
                // boolean "enabled" is a failed lookup, not a default to cache (#112).
                JsonElement enabled = data == null ? null : data.get("enabled");
                if (enabled == null || !enabled.isJsonPrimitive()
                        || !enabled.getAsJsonPrimitive().isBoolean()) {
                    throw new IOException("The evaluate response has no boolean \"enabled\"");
                }
                boolean value = enabled.getAsBoolean();

                // The server sends both fields, as null when the flag has none.
                Object configuration = presentOrNull(data.get("configuration"));
                JsonElement variationElement = presentOrNull(data.get("variation"));
                String variation = variationElement == null ? null : variationElement.getAsString();

                // Parse everything before caching, so a failure here caches nothing.
                FlagEvaluationResult.Metadata metadata = new FlagEvaluationResult.Metadata();
                JsonElement flagId = presentOrNull(data.get("flagId"));
                if (flagId != null) metadata.setFlagId(flagId.getAsString());
                JsonElement description = presentOrNull(data.get("description"));
                if (description != null) metadata.setDescription(description.getAsString());
                JsonElement variant = presentOrNull(data.get("variant"));
                if (variant != null) metadata.setVariant(variant.getAsString());

                // Cache the result (including configuration and variation)
                setCachedValue(flagKey, value, configuration, variation);

                // Apply overrides to evaluated result
                ConfigOverrideEntry configOverride = configOverrides.get(flagKey);
                if (configOverride != null) {
                    if (configOverride.merge && configuration != null) {
                        configuration = mergeConfigurations(configuration, configOverride.config);
                    } else {
                        configuration = configOverride.config;
                    }
                }

                VariationOverrideEntry variationOverride = variationOverrides.get(flagKey);
                if (variationOverride != null) {
                    variation = variationOverride.variation;
                }

                logger.debug("Flag '{}' evaluated: {}", flagKey, value);
                return new FlagEvaluationResult(flagKey, value, configuration, variation, "evaluated", metadata);
            }
        } catch (Exception e) {
            logger.error("Error evaluating flag '{}': {}", flagKey, e.getMessage());
            config.getOnError().accept(e instanceof Exception ? (Exception) e : new Exception(e));

            // Return default value
            boolean defaultValue = defaultFor(flagKey);
            return new FlagEvaluationResult(flagKey, defaultValue, "error", null);
        }
    }

    /**
     * Returns whether a flag key may be sent to the API.
     *
     * <p>A key is valid when it is 1 to 128 characters long, every character is in
     * {@code [A-Za-z0-9._-]}, and it is not {@code .} or {@code ..}. This mirrors the
     * server's flag key rule. The key becomes one URL path segment carrying the SDK key in
     * the Authorization header; a key containing {@code /}, {@code ?} or {@code #} could
     * steer the request, and that header, to another endpoint, and {@code .} or {@code ..}
     * cannot be carried in a URL path because clients normalize them away (#107).
     *
     * @param key the flag key, possibly null
     * @return true if the key is safe to use as a path segment
     */
    private static boolean isValidFlagKey(String key) {
        if (key == null || key.isEmpty() || key.length() > 128) {
            return false;
        }
        if (key.equals(".") || key.equals("..")) {
            return false;
        }
        for (int i = 0; i < key.length(); i++) {
            char c = key.charAt(i);
            boolean ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
                    || (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-';
            if (!ok) {
                return false;
            }
        }
        return true;
    }

    private String describeInvalidFlagKey(String key) {
        if (key == null) {
            return "Invalid flag key: null";
        }
        String prefix = key.length() > 64 ? key.substring(0, 64) : key;
        return "Invalid flag key " + gson.toJson(prefix) + " (length " + key.length()
                + "): keys must be 1-128 characters of A-Z a-z 0-9 . _ - and not . or ..";
    }

    /** {@code element}, or null when it is absent or JSON {@code null}. */
    private static JsonElement presentOrNull(JsonElement element) {
        return element == null || element.isJsonNull() ? null : element;
    }

    private boolean defaultFor(String flagKey) {
        Map<String, Boolean> defaults = config.getDefaults();
        if (flagKey == null || defaults == null) {
            return false;
        }
        Boolean value = defaults.get(flagKey);
        return value != null ? value : false;
    }

    /**
     * Check if a flag is enabled (convenience method)
     */
    public boolean isEnabled(String flagKey, FlagContext context) {
        return evaluate(flagKey, context).getValue();
    }

    /**
     * Check if a flag is enabled without context
     */
    public boolean isEnabled(String flagKey) {
        return isEnabled(flagKey, null);
    }

    /**
     * Get dynamic configuration for a flag (Phase 1)
     * Returns configuration if flag is enabled, otherwise returns null
     */
    public Object getConfig(String flagKey, FlagContext context) {
        FlagEvaluationResult result = evaluate(flagKey, context);
        if (!result.getValue()) {
            return null;
        }
        return result.getConfiguration();
    }

    /**
     * Get dynamic configuration without context
     */
    public Object getConfig(String flagKey) {
        return getConfig(flagKey, null);
    }

    /**
     * Get dynamic configuration with type casting (Phase 1)
     */
    public <T> T getConfig(String flagKey, FlagContext context, Class<T> type) {
        Object config = getConfig(flagKey, context);
        if (config == null) {
            return null;
        }
        return gson.fromJson(gson.toJson(config), type);
    }

    /**
     * Get variation details for multi-variant flags (Phase 2)
     * Returns variation name, enabled status, and configuration
     */
    public VariationResult getVariation(String flagKey, FlagContext context) {
        FlagEvaluationResult result = evaluate(flagKey, context);
        String variation = result.getVariation() != null ? result.getVariation() : "control";
        return new VariationResult(variation, result.getValue(), result.getConfiguration());
    }

    /**
     * Get variation without context
     */
    public VariationResult getVariation(String flagKey) {
        return getVariation(flagKey, null);
    }

    /**
     * Invalidate cache for a specific flag
     */
    public void invalidateCache(String flagKey) {
        if (flagKey != null) {
            cache.remove(flagKey);
        } else {
            cache.clear();
        }
    }

    private CacheEntry getCachedEntry(String flagKey) {
        CacheEntry entry = cache.get(flagKey);
        if (entry == null) return null;

        if (System.currentTimeMillis() > entry.expiresAt) {
            cache.remove(flagKey);
            return null;
        }

        return entry;
    }

    private void setCachedValue(String flagKey, boolean value, Object configuration, String variation) {
        cache.put(flagKey, new CacheEntry(value, configuration, variation,
                System.currentTimeMillis() + config.getCacheTtl()));
    }

    /**
     * Set a configuration override for a flag
     * Useful for testing different configuration values without server changes
     */
    public void setConfigOverride(String flagKey, Object config, boolean merge) {
        configOverrides.put(flagKey, new ConfigOverrideEntry(config, merge, System.currentTimeMillis()));
        invalidateCache(flagKey);
    }

    /**
     * Set a configuration override with default merge=false
     */
    public void setConfigOverride(String flagKey, Object config) {
        setConfigOverride(flagKey, config, false);
    }

    /**
     * Clear configuration override for a flag
     */
    public void clearConfigOverride(String flagKey) {
        configOverrides.remove(flagKey);
        invalidateCache(flagKey);
    }

    /**
     * Set a variation override for a multi-variant flag
     * Forces the flag to return a specific variation
     */
    public void setVariationOverride(String flagKey, String variation) {
        variationOverrides.put(flagKey, new VariationOverrideEntry(variation, System.currentTimeMillis()));
        invalidateCache(flagKey);
    }

    /**
     * Clear variation override for a flag
     */
    public void clearVariationOverride(String flagKey) {
        variationOverrides.remove(flagKey);
        invalidateCache(flagKey);
    }

    /**
     * Check if a flag has a configuration override
     */
    public boolean hasConfigOverride(String flagKey) {
        return configOverrides.containsKey(flagKey);
    }

    /**
     * Check if a flag has a variation override
     */
    public boolean hasVariationOverride(String flagKey) {
        return variationOverrides.containsKey(flagKey);
    }

    /**
     * Get all configuration overrides (for debugging/inspection)
     */
    public Map<String, Object> getConfigOverrides() {
        Map<String, Object> overrides = new HashMap<>();
        configOverrides.forEach((key, entry) -> {
            Map<String, Object> value = new HashMap<>();
            value.put("config", entry.config);
            value.put("merge", entry.merge);
            value.put("timestamp", entry.timestamp);
            overrides.put(key, value);
        });
        return overrides;
    }

    /**
     * Get all variation overrides (for debugging/inspection)
     */
    public Map<String, Object> getVariationOverrides() {
        Map<String, Object> overrides = new HashMap<>();
        variationOverrides.forEach((key, entry) -> {
            Map<String, Object> value = new HashMap<>();
            value.put("variation", entry.variation);
            value.put("timestamp", entry.timestamp);
            overrides.put(key, value);
        });
        return overrides;
    }

    /**
     * Clear all configuration and variation overrides
     */
    public void clearAllOverrides() {
        configOverrides.clear();
        variationOverrides.clear();
        cache.clear();
    }

    /**
     * Merge two configuration objects (for partial overrides)
     * Deep merge where override values take precedence
     */
    @SuppressWarnings("unchecked")
    private Object mergeConfigurations(Object base, Object override) {
        if (base == null) return override;
        if (override == null) return override;

        if (base instanceof Map && override instanceof Map) {
            Map<String, Object> baseMap = new HashMap<>((Map<String, Object>) base);
            Map<String, Object> overrideMap = (Map<String, Object>) override;

            for (Map.Entry<String, Object> entry : overrideMap.entrySet()) {
                String key = entry.getKey();
                Object overrideValue = entry.getValue();

                if (baseMap.containsKey(key)) {
                    Object baseValue = baseMap.get(key);
                    if (baseValue instanceof Map && overrideValue instanceof Map) {
                        baseMap.put(key, mergeConfigurations(baseValue, overrideValue));
                        continue;
                    }
                }
                baseMap.put(key, overrideValue);
            }

            return baseMap;
        }

        return override;
    }

    @Override
    public void close() {
        cache.clear();
        httpClient.dispatcher().executorService().shutdown();
        httpClient.connectionPool().evictAll();
        logger.info("Otto Flags FlagClient closed");
    }

    private static class CacheEntry {
        final boolean value;
        final Object configuration;
        final String variation;
        final long expiresAt;

        CacheEntry(boolean value, Object configuration, String variation, long expiresAt) {
            this.value = value;
            this.configuration = configuration;
            this.variation = variation;
            this.expiresAt = expiresAt;
        }
    }

    private static class ConfigOverrideEntry {
        final Object config;
        final boolean merge;
        final long timestamp;

        ConfigOverrideEntry(Object config, boolean merge, long timestamp) {
            this.config = config;
            this.merge = merge;
            this.timestamp = timestamp;
        }
    }

    private static class VariationOverrideEntry {
        final String variation;
        final long timestamp;

        VariationOverrideEntry(String variation, long timestamp) {
            this.variation = variation;
            this.timestamp = timestamp;
        }
    }
}
