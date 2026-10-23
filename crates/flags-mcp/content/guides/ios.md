# otto-flags iOS SDK (app: {{APP_NAME}})

`OttoFlagsSDK` is the Swift client SDK for iOS (also macOS, tvOS and watchOS). It runs inside the
shipped app, so it authenticates with the app's public client key, not a secret: the key is shown by
`list_apps` and filled in below when you pass the app to this tool. Never put the app's server key
in a mobile app. It needs iOS 13 or later, Swift 5.9 and Xcode 15. The client names no application
id: the key identifies the app.

## Install

The package is not installable by version from the repository yet, and Swift Package Manager cannot
use a sub-directory of a git repository, so add it as a local package from a checkout, pinned to a
full commit SHA of `savvagent/otto-flags`. Find the current tip of `main` with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

Ask a person to review and approve the SHA before you pin it, or use the SHA of a commit they have
already checked. It must be a commit on `main` of `savvagent/otto-flags`, not one from a fork. Use
the approved SHA in place of `<commit>`:

```bash
git clone https://github.com/savvagent/otto-flags
cd otto-flags
git checkout <commit>
```

The package is the directory `packages/ios-sdk` of that checkout. In Xcode choose File, Add
Package Dependencies, Add Local, and pick that directory, then add the `OttoFlagsSDK` library to
your app target. In a `Package.swift`, point at it by path (relative to your package):

```swift
dependencies: [
    .package(path: "../otto-flags/packages/ios-sdk")
],
targets: [
    .target(
        name: "MyApp",
        dependencies: [.product(name: "OttoFlagsSDK", package: "ios-sdk")]
    )
]
```

Keep the checkout at `<commit>` wherever your app is built (CI included), for example as a git
submodule pinned to that commit.

## Initialize

Create one client for the app's lifetime and reuse it; it holds the cache.

```swift
import OttoFlagsSDK

let config = OttoFlagsConfig(
    sdkKey: "{{CLIENT_KEY}}",
    environment: "production"
)
let client = OttoFlagsClient(config: config)
```

Other `OttoFlagsConfig` parameters, all optional: `apiUrl` (default
`https://otto-flags.savvagent.com`), `pollingInterval` (seconds, default 60), `enableWebSocket` (ignored), `timeout` (seconds, default
30) and `enableRealtime` (default `false`, see below). Pass them in that order. Call
`client.close()` to stop the stream and release the client when the app no longer needs it.

**Environment.** The `environment` this app evaluates in, wherever this guide sets it (the client
configuration or a context), must be one of the app's environments: {{ENVIRONMENTS}}. Read it from the
build's or deployment's configuration instead of hardcoding it.

## Real-time updates

Changes reach the app through the poll (`pollingInterval`) unless you set `enableRealtime: true`,
which also reads the server's flag-change stream and drops a changed flag's cached results within
about a round trip. It is off by default because a held connection costs battery and radio wake-ups.
The stream never delays or fails a lookup; polling remains the fallback while it is down. It runs
while the client is open, so call `close()` when the app goes to the background if you do not want
the connection held (`close()` stops the stream only, polling runs until the client is released; create a new client
on return).

## Evaluate a flag

Describe the user with a `UserContext`. Percentage rollouts bucket on `userId`, so pass a stable
identifier.

```swift
let context = UserContext(userId: "user-123", attributes: ["plan": "pro"])

do {
    if try await client.isEnabled(flagKey: "new-checkout", context: context) {
        showNewCheckout()
    } else {
        showOldCheckout()
    }
} catch {
    showOldCheckout() // the lookup threw: decide what the app does without an answer
}

let result = try await client.evaluate(flagKey: "new-checkout", context: context)
// result.value, result.variation, result.configuration, result.reason
```

Also available: `getConfig(flagKey:context:)` for configuration attached to an enabled flag,
`getVariationDetails(flagKey:context:)` for multi-variant flags, and
`getVariation(flagKey:context:defaultValue:)`, which never throws: with a `String` default it
returns the variation name, with a `Bool` default whether the flag is enabled, and the default on
any failure.

```swift
let enabled = await client.getVariation(
    flagKey: "new-checkout",
    context: context,
    defaultValue: false
)
```

## Frameworks

### SwiftUI

Share one client and evaluate in `.task`:

```swift
let flagClient = OttoFlagsClient(
    config: OttoFlagsConfig(sdkKey: "{{CLIENT_KEY}}")
)

struct CheckoutView: View {
    @State private var newCheckout = false

    var body: some View {
        Group {
            if newCheckout { NewCheckout() } else { OldCheckout() }
        }
        .task {
            newCheckout = await flagClient.getVariation(
                flagKey: "new-checkout",
                context: UserContext(userId: "user-123"),
                defaultValue: false
            )
        }
    }
}
```

## Notes

- `evaluate`, `isEnabled`, `getConfig` and `getVariationDetails` are `async throws`: a network or
  server failure surfaces as an `OttoFlagsError`, so handle it and fall back to a safe default.
  `getVariation(flagKey:context:defaultValue:)` is the variant that swallows failures.
- Results are cached in memory per flag and per user context, so one user's result is never served
  to another. The cache has no expiry; the SDK polls the flag list every `pollingInterval` and
  drops the cached results of the flags it lists, so a flag changed afterwards (including a kill
  switch or rollback) typically reaches a running app within about one polling interval, or within
  about a round trip with `enableRealtime` on (see Real-time updates). That holds while the device
  is online and the app is running, a failed poll is not retried until the next one, and an archived
  or deleted flag drops out of the list, so turn a flag off with `set_flag_environment` rather than
  archiving it when you need a kill switch.
- Flag evaluations are not counted by this SDK and `track(eventName:context:properties:)` posts to an
  endpoint the server does not serve, so `flag_health` and auto-rollback get no data from it.
- Create flags before code references them: an unknown flag key is answered with 404, which the SDK reports as a failed lookup.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
