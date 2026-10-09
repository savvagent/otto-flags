# Otto Flags Android SDK

The official Android SDK for [Otto Flags](https://github.com/savvagent/otto-flags) - the AI-powered feature flag platform that prevents production incidents.

## Features

- **Native Kotlin API** - Idiomatic Kotlin with coroutines support
- **Real-time Updates** - WebSocket support for instant flag updates
- **Automatic Caching** - Smart caching with configurable polling
- **Type-Safe** - Full type safety with Kotlin's type system
- **Lightweight** - Minimal dependencies
- **Lifecycle Aware** - Integrates with Android lifecycle components
- **Flow Support** - Reactive flag updates using Kotlin Flow

## Requirements

- Android 5.0 (API level 21) or higher
- Kotlin 1.9+
- Gradle 8.0+

## Installation

### Gradle (Kotlin DSL)

Add to your `build.gradle.kts`:

```kotlin
dependencies {
    implementation("com.savvagent.ottoflags:android-sdk:0.1.0")
}
```

### Gradle (Groovy)

Add to your `build.gradle`:

```groovy
dependencies {
    implementation 'com.savvagent.ottoflags:android-sdk:0.1.0'
}
```

### Maven

```xml
<dependency>
    <groupId>com.savvagent.ottoflags</groupId>
    <artifactId>android-sdk</artifactId>
    <version>0.1.0</version>
</dependency>
```

## Quick Start

### Basic Usage

```kotlin
import com.savvagent.ottoflags.sdk.*
import kotlinx.coroutines.launch

// Configure the SDK
val config = OttoFlagsConfig(
    apiUrl = "https://otto-flags.savvagent.com",
    sdkKey = "your-sdk-key",
    environment = "production"
)

// Initialize the client
val client = OttoFlagsClient(config, context)

// Create user context
val userContext = UserContext(
    userId = "user-123",
    attributes = mapOf(
        "email" to "user@example.com",
        "plan" to "pro"
    )
)

// Check if a feature is enabled
lifecycleScope.launch {
    val result = client.isEnabled("new-feature", userContext)
    result.onSuccess { isEnabled ->
        if (isEnabled) {
            // Show new feature
        } else {
            // Show old feature
        }
    }
}
```

### Jetpack Compose Integration

```kotlin
import androidx.compose.runtime.*
import androidx.lifecycle.compose.collectAsStateWithLifecycle

@Composable
fun FeatureScreen(client: OttoFlagsClient) {
    val flags by client.flagUpdates.collectAsStateWithLifecycle()
    val isNewUIEnabled = flags["new-ui"] ?: false

    if (isNewUIEnabled) {
        NewUI()
    } else {
        OldUI()
    }
}

// ViewModel integration
class FeatureViewModel(private val client: OttoFlagsClient) : ViewModel() {
    private val userContext = UserContext(userId = "current-user")

    val isFeatureEnabled = MutableLiveData<Boolean>()

    fun checkFeature() {
        viewModelScope.launch {
            val result = client.isEnabled("new-feature", userContext)
            result.onSuccess { enabled ->
                isFeatureEnabled.postValue(enabled)
            }
        }
    }
}
```

### Traditional View Integration

```kotlin
class MainActivity : AppCompatActivity() {
    private lateinit var client: OttoFlagsClient

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val config = OttoFlagsConfig(
            sdkKey = "your-sdk-key",
            environment = "production"
        )
        client = OttoFlagsClient(config, this)

        loadFeatureFlags()
    }

    private fun loadFeatureFlags() {
        lifecycleScope.launch {
            val context = UserContext(userId = "user-123")

            val result = client.isEnabled("new-feature", context)
            result.onSuccess { isEnabled ->
                if (isEnabled) {
                    setupNewFeature()
                } else {
                    setupOldFeature()
                }
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        client.close()
    }
}
```

## Advanced Usage

### Configuration Options

```kotlin
val config = OttoFlagsConfig(
    apiUrl = "https://otto-flags.savvagent.com",
    sdkKey = "your-sdk-key",
    environment = "production",
    pollingInterval = 60_000L,     // Poll every 60 seconds
    enableWebSocket = true,         // Enable real-time updates
    timeout = 30_000L,              // Request timeout in milliseconds
    enableLogging = true            // Enable debug logging
)
```

### Getting Variation Values

```kotlin
// Get string variation
val theme = client.getVariation(
    flagKey = "app-theme",
    userContext = context,
    defaultValue = "light"
)

// Get numeric variation
val maxItems = client.getVariation(
    flagKey = "max-items",
    userContext = context,
    defaultValue = 10
)

// Get map variation
val config = client.getVariation(
    flagKey = "app-config",
    userContext = context,
    defaultValue = mapOf("key" to "value")
)
```

### Event Tracking

```kotlin
// Track custom events
lifecycleScope.launch {
    client.track(
        eventName = "feature_used",
        userContext = context,
        properties = mapOf(
            "feature" to "new-checkout",
            "duration" to 1234
        )
    )
}
```

### Real-time Flag Updates

```kotlin
// Observe flag updates using Flow
lifecycleScope.launch {
    client.flagUpdates.collect { flags ->
        val isEnabled = flags["new-feature"] ?: false
        updateUI(isEnabled)
    }
}
```

### Cleanup

```kotlin
// Close the client when done
override fun onDestroy() {
    super.onDestroy()
    client.close()
}
```

## Configuration

### User Context

The `UserContext` object contains information about the user for flag evaluation:

```kotlin
val context = UserContext(
    userId = "user-123",
    attributes = mapOf(
        "email" to "user@example.com",
        "plan" to "pro",
        "signupDate" to "2024-01-01",
        "country" to "US",
        "customAttribute" to true
    )
)
```

### WebSocket Support

Enable WebSocket for real-time flag updates:

```kotlin
val config = OttoFlagsConfig(
    sdkKey = "your-sdk-key",
    enableWebSocket = true  // Flags update in real-time
)
```

### Caching

The SDK automatically caches flag evaluations to improve performance. Configure polling interval:

```kotlin
val config = OttoFlagsConfig(
    sdkKey = "your-sdk-key",
    pollingInterval = 30_000L  // Poll every 30 seconds
)
```

## Testing

The SDK includes comprehensive unit tests:

```bash
./gradlew test
```

Run instrumentation tests:

```bash
./gradlew connectedAndroidTest
```

## Error Handling

The SDK uses Kotlin's `Result` type for error handling:

```kotlin
val result = client.isEnabled("feature", context)

result
    .onSuccess { isEnabled ->
        // Handle success
    }
    .onFailure { error ->
        when (error) {
            is OttoFlagsException -> {
                // Handle Otto Flags-specific error
            }
            else -> {
                // Handle other errors
            }
        }
    }
```

## ProGuard / R8

If you're using ProGuard or R8, add these rules to your `proguard-rules.pro`:

```proguard
# Otto Flags SDK
-keep class com.savvagent.ottoflags.sdk.** { *; }
-keepclassmembers class com.savvagent.ottoflags.sdk.** { *; }

# OkHttp
-dontwarn okhttp3.**
-keep class okhttp3.** { *; }

# Kotlin Coroutines
-keepnames class kotlinx.coroutines.internal.MainDispatcherFactory {}
-keepnames class kotlinx.coroutines.CoroutineExceptionHandler {}
```

## Best Practices

1. **Initialize Once** - Create a single client instance and reuse it (consider using Dependency Injection)
2. **Use Context** - Provide rich user context for accurate targeting
3. **Handle Errors** - Always handle potential errors gracefully using Result
4. **Observe Updates** - Use Flow to react to flag changes in real-time
5. **Clean Up** - Call `close()` when your Activity/Fragment is destroyed
6. **Background Thread** - The SDK handles threading automatically with coroutines

## Example App

See the [Android example app](../../examples/android-app) for a complete implementation.

## Documentation

- [API Reference](https://github.com/savvagent/otto-flags/tree/main/packages/android-sdk)
- [Integration Guide](https://github.com/savvagent/otto-flags/tree/main/packages/android-sdk)
- [Migration Guide](https://github.com/savvagent/otto-flags/tree/main/packages/android-sdk)

## Support

- [GitHub Issues](https://github.com/savvagent/otto-flags/issues)
- [Documentation](https://github.com/savvagent/otto-flags#readme)
- [Email](mailto:support@savvagent.com)

## License

MIT License - see [LICENSE](../../LICENSE) for details
