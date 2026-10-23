# otto-flags Java SDK (app: {{APP_NAME}})

`com.savvagent.ottoflags:otto-flags-java-server-sdk` evaluates flags for a JVM service
(package `com.savvagent.ottoflags.sdk`). This SDK accepts only the app's public client key (it
rejects any other key when the config is built), so it is not a secret; it is shown by `list_apps`
and filled in below when you pass the app to this tool. The app's server key is not used here.
Java 11 or later.

## Install

The artifact is not installable by name yet. Build it from source at a full commit SHA of
`savvagent/otto-flags` into your local Maven repository (`~/.m2`). Find the current tip of `main`
with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

Ask a person to review and approve the SHA before you pin it, or use the SHA of a commit they have
already checked. It must be a commit on `main` of `savvagent/otto-flags`, not one from a fork. Use
the approved SHA in place of `<commit>`. You need `git`, a JDK and Maven:

```bash
git clone https://github.com/savvagent/otto-flags
cd otto-flags
git checkout <commit>
cd packages/java-server
mvn -B install -DskipTests -Dmaven.javadoc.skip=true
```

Then depend on the local coordinates in your own project. Maven finds them in `~/.m2` on the
machine that ran the build:

```xml
<dependency>
    <groupId>com.savvagent.ottoflags</groupId>
    <artifactId>otto-flags-java-server-sdk</artifactId>
    <version>0.1.0</version>
</dependency>
```

Gradle must be told to look there:

```groovy
repositories {
    mavenLocal()
    // your other repositories
}

dependencies {
    implementation 'com.savvagent.ottoflags:otto-flags-java-server-sdk:0.1.0'
}
```

Every machine that builds your project (CI included) needs the same install step at the same
commit. The version number is not unique per commit, so keep `<commit>` recorded in your build
documentation.

## Initialize

Create one client per process and reuse it (a Spring `@Bean`, for example) and close it on
shutdown.

```java
import com.savvagent.ottoflags.sdk.FlagClient;
import com.savvagent.ottoflags.sdk.FlagClientConfig;

import java.util.Map;

FlagClientConfig config = FlagClientConfig.builder("{{CLIENT_KEY}}")
    .applicationId("{{APP_ID}}")
    .defaults(Map.of("new-checkout", false))
    .onError(error -> logger.warn("otto-flags error", error))
    .build();

FlagClient client = new FlagClient(config);
// ... on shutdown:
client.close();
```

`FlagClient` is `AutoCloseable`, so `try (FlagClient client = new FlagClient(config)) { ... }`
works too. Other builder methods, all optional: `baseUrl` (default
`https://otto-flags.savvagent.com`), `cacheTtl` (milliseconds, default 60000), `timeout`
(milliseconds, default 5000), `enableRealtime` (default on, see below) and `defaults` (flag key to
the value served when evaluation fails).

**Environment.** The `environment` this app evaluates in, set in the configuration above or on a
context, must be one of the app's environments: {{ENVIRONMENTS}}. A deployed service should read it from
its own configuration instead of hardcoding it.

## Evaluate a flag

Pass a `FlagContext` describing who the evaluation is for. Percentage rollouts bucket on the user id
(or session id), so pass a stable identifier.

```java
import com.savvagent.ottoflags.sdk.FlagContext;
import com.savvagent.ottoflags.sdk.FlagEvaluationResult;

FlagContext context = FlagContext.builder()
    .userId("user-123")
    .environment("production")
    .attribute("plan", "pro")
    .build();

if (client.isEnabled("new-checkout", context)) {
    renderNewCheckout();
} else {
    renderOldCheckout();
}

FlagEvaluationResult result = client.evaluate("new-checkout", context);
// result.getValue(), result.getVariation(), result.getConfiguration(), result.getReason()
```

Also available: `getConfig(flagKey, context)` for configuration attached to a flag and
`getVariation(flagKey, context)` for multi-variant flags. `invalidateCache("flag-key")` drops a
flag's cached results; `invalidateCache(null)` drops them all.

Results are cached per flag and per evaluation context for the cache TTL, so two users never share
a cached result.

## Frameworks

### Spring Boot

```java
@Configuration
public class OttoFlagsConfig {
    @Bean(destroyMethod = "close")
    public FlagClient flagClient(
            @Value("${ottoFlags.client-key}") String clientKey,
            @Value("${ottoFlags.app-id}") String appId) {
        return new FlagClient(FlagClientConfig.builder(clientKey).applicationId(appId).build());
    }
}

@Service
public class CheckoutService {
    private final FlagClient flags;

    public CheckoutService(FlagClient flags) {
        this.flags = flags;
    }

    public boolean useNewCheckout(String userId) {
        return flags.isEnabled("new-checkout", FlagContext.builder().userId(userId).build());
    }
}
```

## Notes

- A flag lookup does not throw on a network or server error: `evaluate` returns the value from
  `defaults` (or `false`) with `getReason()` equal to `"error"` and passes the exception to
  `onError`. An invalid flag key (1 to 128 ASCII letters, digits, `-`, `_` and `.`) is refused the
  same way without a request. Building the config with a missing or malformed key throws
  `IllegalArgumentException`, so a misconfigured deploy fails at start-up.
- `enableTelemetry` is a builder option, but this SDK does not act on it: it reports no evaluation
  counts or errors, so `flag_health` and auto-rollback get no data from it.
- Real-time updates are on by default (`enableRealtime(false)` turns them off). The client reads the
  server's flag-change stream on a daemon thread and drops a changed flag's cached results, so a
  change made with `set_flag_environment` or `rollback_flag` reaches the app within about a round
  trip. The stream never delays or fails a lookup; while it is down, a change reaches the app when
  the cached result expires. It reconnects with backoff and drops the whole cache on every
  connection. `close()` stops it, and `onError` may be called from its thread.
- Create flags before code references them: an unknown flag key evaluates to its default.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
