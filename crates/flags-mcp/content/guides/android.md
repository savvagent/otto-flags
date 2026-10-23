# otto-flags Android SDK (app: {{APP_NAME}})

`com.savvagent.ottoflags:android-sdk` is the Kotlin client SDK for Android (package
`com.savvagent.ottoflags.sdk`). It runs inside the shipped app, so it authenticates with the app's
public client key, not a secret: the key is shown by `list_apps` and filled in below when you pass
the app to this tool. Never put the app's server key in a mobile app. It needs Android 5.0 (API 21)
or later and Kotlin 1.9 or later. The client names no application id: the key identifies the app.

## Install

The artifact is not installable by name yet. Build it from source at a full commit SHA of
`savvagent/otto-flags` into your local Maven repository (`~/.m2`). Find the current tip of `main`
with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

Ask a person to review and approve the SHA before you pin it, or use the SHA of a commit they have
already checked. It must be a commit on `main` of `savvagent/otto-flags`, not one from a fork. Use
the approved SHA in place of `<commit>`. You need `git`, a JDK 17, the Android SDK and Gradle 8 or
later; the repository has no Gradle wrapper, so use your own `gradle`:

```bash
git clone https://github.com/savvagent/otto-flags
cd otto-flags
git checkout <commit>
cd packages/android-sdk
gradle publishToMavenLocal
```

Then add `mavenLocal()` to the repositories your app resolves from and depend on the local
coordinates. If your `settings.gradle.kts` uses `dependencyResolutionManagement`, add it there:

```kotlin
dependencyResolutionManagement {
    repositories {
        mavenLocal()
        google()
        // your other repositories
    }
}
```

```kotlin
dependencies {
    implementation("com.savvagent.ottoflags:android-sdk:0.1.0")
}
```

Every machine that builds your app (CI included) needs the same install step at the same commit.
The version number is not unique per commit, so keep `<commit>` recorded in your build
documentation.

## Initialize

Create one client for the app's lifetime (an application class or a dependency-injection singleton)
and reuse it; it holds the cache.

```kotlin
import com.savvagent.ottoflags.sdk.OttoFlagsClient
import com.savvagent.ottoflags.sdk.OttoFlagsConfig

val config = OttoFlagsConfig(
    sdkKey = "{{CLIENT_KEY}}",
    environment = "production"
)
val client = OttoFlagsClient(config, applicationContext)
```

Other `OttoFlagsConfig` parameters, all optional: `apiUrl` (default
`https://otto-flags.savvagent.com`), `pollingInterval` (milliseconds, default 60000),
`enableWebSocket` (default `true`), `timeout` (milliseconds, default 30000) and `enableLogging`
(default `false`). Call `client.close()` when the app no longer needs the client.

**Environment.** The `environment` this app evaluates in, set in the configuration above or on a
context, must be one of the app's environments: {{ENVIRONMENTS}}. A deployed service should read it from
its own configuration instead of hardcoding it.

## Evaluate a flag

Describe the user with a `UserContext`. Percentage rollouts bucket on `userId`, so pass a stable
identifier. The lookups are `suspend` functions that return a `Result`:

```kotlin
import com.savvagent.ottoflags.sdk.UserContext

val user = UserContext(userId = "user-123", attributes = mapOf("plan" to "pro"))

lifecycleScope.launch {
    val enabled = client.isEnabled("new-checkout", user).getOrDefault(false)
    if (enabled) showNewCheckout() else showOldCheckout()
}

val result = client.evaluate("new-checkout", user)
    .getOrNull() // FlagEvaluationResult: value, variation, configuration, reason
```

Also available: `getConfig(flagKey, userContext)` for configuration attached to an enabled flag,
`getVariationDetails(flagKey, userContext)` for multi-variant flags, and
`getVariation(flagKey, userContext, defaultValue)`, which returns the variation name for a `String`
default, whether the flag is enabled for a `Boolean` default, and the default on any failure.

## Frameworks

### Jetpack Compose

Evaluate in a `LaunchedEffect` and keep the answer in state:

```kotlin
@Composable
fun CheckoutScreen(client: OttoFlagsClient, userId: String) {
    var newCheckout by remember { mutableStateOf(false) }

    LaunchedEffect(userId) {
        newCheckout = client.getVariation("new-checkout", UserContext(userId), false)
    }

    if (newCheckout) NewCheckout() else OldCheckout()
}
```

### ViewModel

```kotlin
class CheckoutViewModel(private val client: OttoFlagsClient) : ViewModel() {
    val newCheckout = MutableLiveData(false)

    fun load(userId: String) {
        viewModelScope.launch {
            newCheckout.postValue(client.getVariation("new-checkout", UserContext(userId), false))
        }
    }
}
```

## Notes

- Lookups return a `Result`, so a network or server failure arrives as `onFailure`; use
  `getOrDefault`, or `getVariation`, which falls back to the default you pass.
- Results are cached in memory per flag and per user context, so one user's result is never served
  to another. The cache has no expiry and this SDK does not refresh it from the server, so a flag
  changed afterwards (including a kill switch or rollback) reaches a running app only after it
  restarts or the user context changes. `client.flagUpdates` is a `StateFlow` of the latest value of
  each flag the app has evaluated.
- Flag evaluations are not counted by this SDK and `track(eventName, userContext, properties)` posts
  to an endpoint the server does not serve, so `flag_health` and auto-rollback get no data from it.
- Create flags before code references them: an unknown flag key is answered with 404, which the SDK reports as a failed lookup.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
