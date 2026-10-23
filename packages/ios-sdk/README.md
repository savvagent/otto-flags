# Otto Flags iOS SDK

The official iOS SDK for [Otto Flags](https://github.com/savvagent/otto-flags) - the AI-powered feature flag platform that prevents production incidents.

## Features

- **Native Swift API** - Idiomatic Swift with async/await support
- **Polling Refresh** - the flag list is polled from `GET /api/sdk/flags`, with an optional real-time stream (`enableRealtime`)
- **Automatic Caching** - Smart caching with configurable polling
- **Type-Safe** - Full type safety with Swift's type system
- **Lightweight** - Zero external dependencies
- **Cross-Platform** - Works on iOS, macOS, tvOS, and watchOS

## Requirements

- iOS 13.0+ / macOS 10.15+ / tvOS 13.0+ / watchOS 6.0+
- Swift 5.9+
- Xcode 15.0+

## Installation

### Swift Package Manager

Add the following to your `Package.swift` file:

```swift
dependencies: [
    .package(url: "https://github.com/savvagent/otto-flags", from: "0.1.0")
]
```

Or in Xcode:

1. File > Add Package Dependencies
2. Enter package URL: `https://github.com/savvagent/otto-flags`
3. Select `OttoFlagsSDK` package

### CocoaPods

```ruby
pod 'OttoFlagsSDK', '~> 0.1.0'
```

### Carthage

```
github "ottoFlags/otto-flags" ~> 0.1.0
```

## Quick Start

### Basic Usage

```swift
import OttoFlagsSDK

// Configure the SDK
let config = OttoFlagsConfig(
    apiUrl: "https://otto-flags.savvagent.com",
    sdkKey: "your-sdk-key",
    environment: "production"
)

// Initialize the client
let client = OttoFlagsClient(config: config)

// Create user context
let context = UserContext(
    userId: "user-123",
    attributes: [
        "email": "user@example.com",
        "plan": "pro"
    ]
)

// Check if a feature is enabled
do {
    let isEnabled = try await client.isEnabled(
        flagKey: "new-feature",
        context: context
    )

    if isEnabled {
        // Show new feature
    } else {
        // Show old feature
    }
} catch {
    print("Error checking flag: \(error)")
}
```

### SwiftUI Integration

```swift
import SwiftUI
import OttoFlagsSDK

struct ContentView: View {
    @StateObject private var featureFlags = FeatureFlagManager()

    var body: some View {
        VStack {
            if featureFlags.isEnabled("new-ui") {
                NewUIView()
            } else {
                OldUIView()
            }
        }
        .task {
            await featureFlags.load()
        }
    }
}

@MainActor
class FeatureFlagManager: ObservableObject {
    @Published var flags: [String: Bool] = [:]
    private let client: OttoFlagsClient

    init() {
        let config = OttoFlagsConfig(
            sdkKey: "your-sdk-key",
            environment: "production"
        )
        self.client = OttoFlagsClient(config: config)
    }

    func isEnabled(_ key: String) -> Bool {
        return flags[key] ?? false
    }

    func load() async {
        let context = UserContext(userId: "current-user")

        do {
            flags["new-ui"] = try await client.isEnabled(
                flagKey: "new-ui",
                context: context
            )
        } catch {
            print("Error loading flags: \(error)")
        }
    }
}
```

### UIKit Integration

```swift
import UIKit
import OttoFlagsSDK

class ViewController: UIViewController {
    private let client: OttoFlagsClient

    init() {
        let config = OttoFlagsConfig(
            sdkKey: "your-sdk-key",
            environment: "production"
        )
        self.client = OttoFlagsClient(config: config)
        super.init(nibName: nil, bundle: nil)
    }

    override func viewDidLoad() {
        super.viewDidLoad()

        Task {
            await loadFeatureFlags()
        }
    }

    func loadFeatureFlags() async {
        let context = UserContext(userId: "user-123")

        do {
            let showNewFeature = try await client.isEnabled(
                flagKey: "new-feature",
                context: context
            )

            if showNewFeature {
                setupNewFeature()
            } else {
                setupOldFeature()
            }
        } catch {
            print("Error: \(error)")
        }
    }
}
```

## Advanced Usage

### Configuration Options

```swift
let config = OttoFlagsConfig(
    apiUrl: "https://otto-flags.savvagent.com",
    sdkKey: "your-sdk-key",
    environment: "production",
    pollingInterval: 60,        // Poll every 60 seconds
    enableWebSocket: true,       // Ignored: no WebSocket endpoint exists
    timeout: 30,                 // Request timeout in seconds
    enableRealtime: true         // default: false; true opens the SSE update stream
)
```

### Getting Variation Values

`getVariation` evaluates the flag. With a `String` default it returns the variation name (or
the default when the flag has no variation); with a `Bool` default it returns whether the flag is
enabled. Any other type, and any failure, returns the default. Use `getConfig` for a flag's
configuration.

```swift
// Variation name, or "light" when the flag has none
let theme = await client.getVariation(
    flagKey: "app-theme",
    context: context,
    defaultValue: "light"
)

// Enabled state, or false on failure
let enabled = await client.getVariation(
    flagKey: "new-checkout",
    context: context,
    defaultValue: false
)
```

### Event Tracking

`track` is a documented no-op (savvagent/otto-flags#125). The server has no custom-event endpoint (only `POST /api/telemetry/evaluations` and `/errors`), so nothing is sent. The method is kept so existing code compiles.

### Cleanup

```swift
// Close the client when done
client.close()
```

## Configuration

### User Context

The `UserContext` object contains information about the user for flag evaluation:

```swift
let context = UserContext(
    userId: "user-123",
    attributes: [
        "email": "user@example.com",
        "plan": "pro",
        "signupDate": "2024-01-01",
        "country": "US",
        "customAttribute": true
    ]
)
```

### Real-time Updates

The server has no WebSocket endpoint (savvagent/otto-flags#125); `enableWebSocket` is ignored and kept only so existing code compiles. The real-time channel is the SSE stream `GET /api/flags/stream`, which the SDK reads when you set `enableRealtime: true`. It is **off by default** because a held connection costs battery and radio wake-ups on a phone.

With it on, the client reads the stream with a `URLSessionDataDelegate` (so the declared iOS 13 minimum is unchanged) and drops a changed flag's cached results as soon as the change arrives, so a kill switch does not wait for the next poll. Polling stays on as the fallback: while the stream is down, or with `enableRealtime` off, changes reach the client through the polling interval (each poll calls `GET /api/sdk/flags?environment=<env>` and drops the cached results of the flags it lists). The stream never delays or fails a lookup. It reconnects with exponential backoff (about 1 second up to 30 seconds, 5 minutes if the key is rejected) and drops the whole cache on every connection, since changes may have been missed. It runs while the client is open, so call `close()` when the app goes to the background if you do not want the connection held; `close()` stops the stream only (polling runs until the client is released), so drop the old client and create a new one when the app comes back.

### Caching

The SDK automatically caches flag evaluations to improve performance, per flag and per user context, so one user's result is never served to another. Configure polling interval:

```swift
let config = OttoFlagsConfig(
    sdkKey: "your-sdk-key",
    pollingInterval: 30  // Poll every 30 seconds
)
```

## Testing

The SDK includes comprehensive unit tests:

```bash
swift test
```

## Error Handling

The SDK defines the following errors:

```swift
enum OttoFlagsError: Error {
    case invalidURL
    case requestFailed
    case invalidResponse
    case networkError(Error)
}
```

Handle errors appropriately:

```swift
do {
    let isEnabled = try await client.isEnabled(
        flagKey: "feature",
        context: context
    )
} catch OttoFlagsError.requestFailed {
    print("Request failed")
} catch OttoFlagsError.invalidResponse {
    print("Invalid response")
} catch {
    print("Unknown error: \(error)")
}
```

## Best Practices

1. **Initialize Once** - Create a single client instance and reuse it
2. **Use Context** - Provide rich user context for accurate targeting
3. **Handle Errors** - Always handle potential errors gracefully
4. **Cache Results** - Let the SDK handle caching automatically
5. **Clean Up** - Call `close()` when your app terminates

## Example App

See the [iOS example app](../../examples/ios-app) for a complete implementation.

## Documentation

- [API Reference](https://github.com/savvagent/otto-flags/tree/main/packages/ios-sdk)
- [Integration Guide](https://github.com/savvagent/otto-flags/tree/main/packages/ios-sdk)
- [Migration Guide](https://github.com/savvagent/otto-flags/tree/main/packages/ios-sdk)

## Support

- [GitHub Issues](https://github.com/savvagent/otto-flags/issues)
- [Documentation](https://github.com/savvagent/otto-flags#readme)
- [Email](mailto:support@savvagent.com)

## License

MIT License - see [LICENSE](../../LICENSE) for details
