# Otto Flags SDK Developer Guide

This guide is for developers building SDKs that integrate with the Otto Flags feature flag platform. It covers the API endpoints, authentication, real-time updates, and best practices.

> **Note:** For official SDKs and examples, see the [otto-flags repository](https://github.com/savvagent/otto-flags).

> **Wiring an existing SDK into a project?** An agent connected over MCP should call
> `sdk_guide(language, app?)`; otherwise read [`/llms.txt`](../llms.txt) or
> [`AGENTS.md`](../AGENTS.md). SDKs are installed from a pinned commit of
> savvagent/otto-flags. This document is the REST contract those SDKs implement.

## Table of Contents

- [Authentication](#authentication)
- [Core Endpoints](#core-endpoints)
  - [Flag Evaluation](#flag-evaluation)
  - [List All Flags (for Local Overrides)](#list-all-flags-for-local-overrides)
  - [Real-time Updates (SSE)](#real-time-updates-sse)
  - [Telemetry](#telemetry)
- [Backend Performance Features](#backend-performance-features)
- [SDK Architecture Recommendations](#sdk-architecture-recommendations)
- [Error Handling](#error-handling)
- [Best Practices](#best-practices)

---

## Authentication

Otto Flags uses two types of API keys:

| Key Type | Prefix | Use Case | Security Level |
|----------|--------|----------|----------------|
| **SDK Key** | `sdk_` | Client-side apps (browsers, mobile) | Public - safe to embed |
| **Server Key** | `srv_` | Server-side apps (Node.js, Python) | Secret - never expose |

### Passing Authentication

All SDK endpoints accept authentication via headers:

```http
# Recommended: Authorization header
Authorization: Bearer sdk_your_key_here

# Alternative: X-SDK-Key header
X-SDK-Key: sdk_your_key_here
```

**Important:** Never pass API keys as query parameters. This prevents credentials from appearing in logs, browser history, and referrer headers.

---

## Core Endpoints

### Flag Evaluation

**Endpoint:** `POST /api/flags/{key}/evaluate`

**Authentication:** Required (SDK Key or JWT)

Evaluates a feature flag for a given context.

`{key}` is the flag key as one percent-encoded path segment. Flag keys are 1-128 ASCII letters,
digits, `-`, `_` and `.`, and never the dot segments `.` or `..` (which no URL path can carry; the
server refuses them at creation). Build the URL from path segments, never by string
concatenation, and treat a key outside that set, or `.` or `..`, as a failed lookup: return the
default without a request.

A `configuration`, the flag's or a variation's, is at most 32 KiB (32 768 bytes) of compact JSON
when written; the server refuses a larger one. An evaluate response carries one configuration
(the matched variation's, else the flag's), so size its read cap well above the limit (the Rust
SDK reads up to 1 MiB) and treat a response over the cap as a failed lookup. Flags stored before
the limit existed may carry more, so do not reject a configuration for exceeding it. The
[list](#list-all-flags-for-local-overrides) response and each [stream](#real-time-updates-sse)
event carry a flag's configuration and every variation's, so they have bounds of their own,
documented there; do not apply the evaluate cap to them.

#### Request

```http
POST /api/flags/dark-mode/evaluate
Content-Type: application/json
Authorization: Bearer sdk_xxx

{
  "context": {
    "user_id": "user-123",
    "environment": "production",
    "attributes": {
      "plan": "premium",
      "country": "US"
    }
  }
}
```

#### Response

```json
{
  "key": "dark-mode",
  "enabled": true,
  "scope": "application",
  "variation": "treatment-a",
  "configuration": {
    "theme": "dark-blue",
    "contrast": "high"
  },
  "timestamp": 1700000000,
  "version": 3,
  "context": { ... }
}
```

#### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| `key` | string | The flag key |
| `enabled` | boolean | Whether the flag is enabled for this context |
| `scope` | string | `"application"` or `"enterprise"` |
| `variation` | string? | The allocated variation name (if A/B testing) |
| `configuration` | object? | Dynamic configuration attached to the flag/variation |
| `timestamp` | number | Unix timestamp of evaluation |
| `version` | number | Flag version that produced this result |

#### Context Fields

| Field | Description |
|-------|-------------|
| `user_id` | Unique user identifier (required for rollouts) |
| `anonymous_id` | Alternative to user_id for anonymous users |
| `session_id` | Fallback identifier |
| `environment` | Target environment (e.g., "production", "staging") |
| `organization_id` | For multi-tenant apps |
| `application_id` | For hierarchical flag lookup |
| `language` | User's language code (e.g., "en", "es") |
| `attributes` | Custom attributes for targeting rules |

---

### List All Flags (for Local Overrides)

SDKs often need to fetch all available flags for local override functionality, offline mode, or initialization. Two endpoints are available:

#### Get Application Flags

**Endpoint:** `GET /api/sdk/flags`

**Authentication:** Required (SDK Key)

Returns all active flags for the SDK key's application, plus any enterprise-scoped flags for the organization.

**Size.** The server refuses a write that would take one flag's environments, variations, and
configurations past 128 KiB (131 072 bytes), or an application's active flags past 1 MiB
(1 048 576 bytes) counting each flag plus 256 bytes for its other fields. Both are measured on
the stored form, which is never shorter than the JSON the server sends. So this response is at
most about 1 MiB plus a small envelope (also at most 4 096 flags). Concurrent writes can take an
application slightly over its budget (each by at most one flag), and flags stored before the
limits may exceed them, so size the read cap with headroom: read up to 4 MiB, and treat a larger
response as a failed refresh (keep the cached flags and the defaults). A server key's response
includes targeting rules and is the larger; a client key's omits them.

##### Request

```http
GET /api/sdk/flags?environment=production
Authorization: Bearer sdk_xxx
```

##### Query Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `environment` | string | `"development"` | Environment to evaluate enabled state for |

##### Response

```json
{
  "flags": [
    {
      "key": "dark-mode",
      "enabled": true,
      "scope": "application",
      "environments": {
        "production": {"enabled": true, "rollout_percentage": 100},
        "development": {"enabled": true, "rollout_percentage": 100}
      },
      "variations": null,
      "configuration": {"theme": "dark-blue"},
      "version": 5
    },
    {
      "key": "maintenance-mode",
      "enabled": false,
      "scope": "enterprise",
      "environments": {
        "production": {"enabled": false}
      },
      "variations": null,
      "version": 2
    }
  ],
  "count": 2,
  "organization_id": "org-uuid",
  "application_id": "app-uuid"
}
```

##### Response Fields

| Field | Type | Description |
|-------|------|-------------|
| `flags` | array | List of flag objects |
| `flags[].key` | string | Flag key |
| `flags[].enabled` | boolean | Enabled state for the requested environment |
| `flags[].scope` | string | `"application"` or `"enterprise"` |
| `flags[].environments` | object | Full environment configuration |
| `flags[].variations` | object? | Variation definitions (for A/B testing) |
| `flags[].configuration` | object? | Dynamic configuration |
| `flags[].version` | number | Flag version (for cache invalidation) |
| `count` | number | Total number of flags returned |
| `organization_id` | string | Organization ID |
| `application_id` | string? | Application ID (present for SDK key auth) |

#### Get Enterprise Flags Only

**Endpoint:** `GET /api/sdk/enterprise-flags`

**Authentication:** Required (SDK Key)

Returns only enterprise-scoped flags for the organization. Enterprise flags are shared across all applications.

##### Request

```http
GET /api/sdk/enterprise-flags?environment=production
Authorization: Bearer sdk_xxx
```

##### Response

Same structure as `/api/sdk/flags`, but only includes flags with `scope: "enterprise"` and `application_id` is always `null`.

#### Use Cases

1. **Local Override UI**: Display all available flags for developers to toggle locally
2. **Offline Mode**: Pre-fetch flags for mobile/desktop apps that need to work offline
3. **SDK Initialization**: Bootstrap SDK with all flag values on startup
4. **DevTools Integration**: Show available flags in browser dev panels

#### SDK Implementation Example

```javascript
class OttoFlagsClient {
  async getAllFlags(environment = 'development') {
    const response = await fetch(
      `${this.baseUrl}/api/sdk/flags?environment=${environment}`,
      { headers: { Authorization: `Bearer ${this.apiKey}` } }
    );
    const { flags } = await response.json();

    // Store in local cache for overrides
    this.flagCache = new Map(flags.map(f => [f.key, f]));
    return flags;
  }

  // Check if flag has local override before evaluating
  async isEnabled(key, context) {
    const override = this.localOverrides.get(key);
    if (override !== undefined) {
      return override; // Use local override
    }
    return this.evaluate(key, context);
  }
}
```

---

### Real-time Updates (SSE)

**Endpoint:** `GET /api/flags/stream`

**Authentication:** Required (SDK Key or JWT)

Server-Sent Events stream for real-time flag change notifications.

Each `flag.created` / `flag.updated` / `flag.deleted` event carries one flag, so it is at most
about 128 KiB (the per-flag limit under [List All Flags](#list-all-flags-for-local-overrides))
plus its envelope. Read events with a cap of 1 MiB, and treat a larger one as missed (the next
poll of the list catches up).

#### Connection

```javascript
// Using @microsoft/fetch-event-source (recommended)
import { fetchEventSource } from '@microsoft/fetch-event-source';

await fetchEventSource('https://flags.savvagent.com/api/flags/stream', {
  method: 'GET',
  headers: {
    'Authorization': `Bearer ${sdkKey}`,
  },
  onopen(response) {
    if (response.ok) {
      console.log('SSE connected');
    }
  },
  onmessage(event) {
    // Handle events
  },
  onerror(err) {
    // Handle errors, implement reconnection
  },
});
```

> **Why not native EventSource?**
> The browser's native `EventSource` API doesn't support custom headers. We require header-based authentication for security (no credentials in URLs).

#### Event Types

| Event | Description | Data |
|-------|-------------|------|
| `connected` | Connection established | `{"message": "...", "organization_id": "...", "application_id": "..."}` |
| `heartbeat` | Keep-alive (every 30s) | `"ping"` |
| `flag.created` | New flag created | [FlagNotification](#flagnotification) |
| `flag.updated` | Flag configuration changed | [FlagNotification](#flagnotification) |
| `flag.deleted` | Flag deleted/archived | [FlagNotification](#flagnotification) |

#### FlagNotification

```json
{
  "type": "flag.updated",
  "key": "dark-mode",
  "organization_id": "org-uuid",
  "application_id": "app-uuid",
  "environments": { ... },
  "status": "active",
  "version": 5,
  "realtime_enabled": true
}
```

#### Tenant Isolation

SSE connections are scoped to your SDK key's organization and application:
- You only receive notifications for flags in your org/app
- Enterprise-scoped flags (no application_id) are broadcast to all apps in the org
- Flags with `realtime_enabled: false` are not broadcast

#### Reconnection Strategy

Implement exponential backoff for reconnection:

```javascript
const reconnect = (attempt) => {
  const delay = Math.min(1000 * Math.pow(2, attempt), 30000);
  setTimeout(() => connectSSE(), delay);
};
```

---

### Telemetry

SDKs should report evaluation metrics and errors back to Otto Flags for analytics.

Report telemetry with the key the SDK already holds: a server SDK sends it with its server key
(`srv_`), a browser or mobile SDK with the public client key (`sdk_`). The examples below show a
server SDK. Both endpoints record which kind of key sent each report (never a field of the
request), and auto-rollback counts only server-key reports by default (see below), so a server
SDK must send evaluations as well as errors with its server key: errors and evaluations are split
by key kind the same way, and a policy compares the two.

Both payloads take an optional `environment` per event (or `context.environment`). Omitted, an
evaluation counts toward `production`; an error is stored without one, which auto-rollback counts
toward `production` and `flag_health` counts in every environment.

#### What is billed

Billing counts evaluations per app and hour, at the larger of two numbers: the evaluations the
server answered (successful responses from `POST /api/flags/{key}/evaluate`, for either key kind)
and the evaluation events accepted from telemetry sent with the **server** key. A server SDK's
telemetry includes the evaluations it answered from its cache or locally, so with telemetry on
(the default) a flag on a hot path counts even when it is served from the cache; with telemetry off
only the server-answered ones count. Reports sent with the public client key are never billed, and
`GET /api/sdk/flags`, the SSE stream and the MCP `evaluate_flag` tool are not counted. The free
allowance is set on the platform and is not recorded in this repository: the `usage` MCP tool shows
this month's meters, the budget and any block. A blocked organization's evaluations keep serving.

Only evaluations in one of the app's own environments count; an evaluation that names any other
environment is answered and not counted. A public client key can be copied from a page, so the
evaluations answered to client keys are billed only up to a ceiling per app and hour (the
server's `FLAGS_CLIENT_EVAL_CEILING_PER_HOUR`, 500000 by default). Evaluations above it are still
answered and are not billed. The `usage` MCP tool splits this month's evaluations by key kind and
shows how many the ceiling left unbilled.

#### Track Evaluations

**Endpoint:** `POST /api/telemetry/evaluations`

Batch evaluation events for analytics.

```http
POST /api/telemetry/evaluations
Content-Type: application/json
Authorization: Bearer srv_xxx

{
  "evaluations": [
    {
      "flag_key": "dark-mode",
      "result": true,
      "user_id": "user-123",
      "environment": "production",
      "context": { ... },
      "timestamp": 1700000000
    }
  ]
}
```

**Best Practice:** Batch evaluations and send every 5-10 seconds to reduce network overhead.

#### Track Errors

**Endpoint:** `POST /api/telemetry/errors`

Report errors that occur while a flag is enabled (for AI-powered correlation).

```http
POST /api/telemetry/errors
Content-Type: application/json
Authorization: Bearer srv_xxx

{
  "errors": [
    {
      "flag_key": "new-checkout",
      "flag_enabled": true,
      "error_type": "TypeError",
      "error_message": "Cannot read property 'x' of undefined",
      "stack_trace": "...",
      "environment": "production",
      "context": { ... },
      "timestamp": 1700000000
    }
  ]
}
```

A browser or mobile SDK sends the same bodies with `Authorization: Bearer sdk_xxx`.

**Auto-rollback:** telemetry reported with the server key (`srv_`) drives auto-rollback: both its
evaluations and its errors. Reports made with the client key (`sdk_`) count only for policies
that opt in (`includeClientReports`, which only an org owner or admin can set), because the
client key is public and anyone holding it can send reports. A server SDK that sends errors with
`srv_` but evaluations with `sdk_` gives a policy errors and no evaluations to compare them with,
so it never trips. Error reports without an `environment` count toward `production`. The wire
format is unchanged: `environment` was already accepted.

---

## Backend Performance Features

The Otto Flags backend includes several performance optimizations that benefit SDK integrations:

### Database Connection Pool

The backend uses a configurable PostgreSQL connection pool to handle concurrent requests:

| Setting | Default | Environment Variable |
|---------|---------|---------------------|
| Max Connections | 20 | `DATABASE_MAX_CONNECTIONS` |
| Acquire Timeout | 10s | `DATABASE_ACQUIRE_TIMEOUT_SECS` |
| Min Connections | 2 | (not configurable) |
| Idle Timeout | 600s | (not configurable) |

**SDK Impact:** With 20 concurrent connections, the backend can handle many simultaneous flag evaluations and SSE connections without blocking.

### SDK Key Caching

SDK key validation is cached in Redis for 5 minutes to reduce database load:

- **Cache TTL:** 300 seconds (5 minutes)
- **Cache Key Pattern:** `sdk_key_cache:{key_preview}`
- **Benefit:** After the first request, subsequent auth validations hit Redis instead of PostgreSQL

This means your SDK's first evaluation or SSE connection may take slightly longer, but subsequent requests will be faster.

### Pool Metrics Logging

The backend logs pool utilization metrics periodically (default: every 30 seconds):

```
pool_size=20 pool_idle=15 pool_active=5 pool_utilization="25.0%"
```

- **< 50% utilization:** Healthy (debug level)
- **50-80% utilization:** Normal load (info level)
- **> 80% utilization:** Under pressure (warning level)

**SDK Impact:** If you see 401 errors or timeouts during high load, it may indicate pool exhaustion on the backend.

### Authentication on Evaluate Endpoint

The `/api/flags/{key}/evaluate` endpoint now requires authentication (SDK key or JWT):

```http
# Required - one of these headers must be present
Authorization: Bearer sdk_xxx
Authorization: Bearer <jwt_token>
X-SDK-Key: sdk_xxx
```

**Breaking Change:** Unauthenticated requests to the evaluate endpoint will now return `401 Unauthorized`.

---

## SDK Architecture Recommendations

### Caching

```
┌─────────────────────────────────────────────┐
│                  SDK                         │
│  ┌─────────────┐    ┌───────────────────┐   │
│  │   Cache     │◄───│  SSE Listener     │   │
│  │  (in-mem)   │    │  (invalidates)    │   │
│  └──────┬──────┘    └───────────────────┘   │
│         │                                    │
│  ┌──────▼──────┐                            │
│  │  Evaluator  │─────► API (on cache miss)  │
│  └─────────────┘                            │
└─────────────────────────────────────────────┘
```

1. **Cache evaluations** with TTL (e.g., 5 minutes)
2. **Invalidate on SSE events** for instant updates: drop every cached entry of the announced
   flag, for all contexts
3. **Key each entry by flag and context** (below), and bound the entries per flag

### Suggested Cache Key Format

An evaluation's answer depends on the flag key, the SDK key (fixed per client) and the whole
context in the request body: `user_id`, every attribute, and every other field a targeting rule
or rollout can read. Key a cached result by the flag key and the full context exactly as sent,
serialized canonically (JSON with object keys sorted at every depth, so attribute order does
not matter):

```
{flag_key} -> {canonical JSON of the request body's "context"} -> result
```

Never key by the flag key alone, or by `user_id` alone: the first context's result would be
returned for every later one (#91, #127). Bound the number of contexts cached per flag (the SDKs
in this repo keep at most 10 000; past that they drop expired entries or clear the flag's
entries, so overflow costs misses, never a wrong answer).

The [list](#list-all-flags-for-local-overrides) response's `enabled` is the flag's switch for the
environment, not any context's result (a flag at a 10 % rollout lists as `enabled: true`), so it
must not fill the evaluation cache.

Keep a generation counter per flag that every invalidation (stream message, poll, override
setter, manual invalidate) bumps. Read it before sending an evaluate request and drop the cache
write if it changed while the request was in flight: that result predates the invalidation, and
caching it would undo a kill switch for that context (#137).

### Offline Support

For mobile/desktop apps:
1. Persist cache to disk (localStorage, SQLite, etc.)
2. Use cached values when offline
3. Sync when connection restored

---

## Error Handling

### HTTP Status Codes

| Code | Meaning | SDK Action |
|------|---------|------------|
| `200` | Success | Use response |
| `401` | Invalid/missing SDK key | Check configuration |
| `403` | A client key used from a browser origin the app does not allow (`origin_not_allowed`) | Return default value; add the origin to the app's allowed origins |
| `404` | Flag not found | Return default value |
| `429` | Rate limited | Back off and retry |
| `500` | Server error | Return cached/default value |
| `503` | Temporarily unavailable or busy (with `Retry-After`) | Return cached/default value; retry after the delay |

### Allowed origins (client keys)

An app can list the browser origins its public client key (`sdk_`) may be used from (`update_app`,
`allowedOrigins`: `https://app.example.com`, or `https://*.example.com` for subdomains). The list is
empty by default, which allows every origin. When it is not empty, `POST /api/evaluate/{key}`,
`POST /api/flags/{key}/evaluate` and `GET /api/sdk/flags` answer `403 origin_not_allowed` to a client
key whose request carries an `Origin` header that is not on the list, and an evaluation refused this
way is not counted. Browsers always send `Origin` on a cross-origin call, so a browser SDK needs no
change beyond the list naming its page's origin. A request with no `Origin` header (a native mobile
app, a server, `curl`) is not checked, and a server key (`srv_`) never is. This keeps other websites
from using a public key; it is not authentication, because a script can send any `Origin` or none.
The flag-change stream and the telemetry routes are not checked.

### Default Values

SDKs should accept default values for graceful degradation:

```javascript
// User provides default
const enabled = await client.isEnabled('new-feature', { default: false });
```

### Timeout Handling

Set reasonable timeouts (e.g., 3 seconds) and fall back to cached/default values:

```javascript
const evaluate = async (key, context, options = {}) => {
  const timeout = options.timeout || 3000;
  const defaultValue = options.default ?? false;

  try {
    const result = await Promise.race([
      fetchEvaluation(key, context),
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error('Timeout')), timeout)
      )
    ]);
    return result.enabled;
  } catch (error) {
    // The cache is keyed by flag and context (see Suggested Cache Key Format)
    return cache.get(key, canonicalContext(context)) ?? defaultValue;
  }
};
```

---

## Best Practices

### 1. Initialize Once

Create a single SDK instance at application startup:

```javascript
// Good
const client = new OttoFlagsClient({ apiKey: 'sdk_xxx' });
export default client;

// Bad - creates multiple connections
function getFlag(key) {
  const client = new OttoFlagsClient({ apiKey: 'sdk_xxx' });
  return client.isEnabled(key);
}
```

### 2. Provide User Context

Always include user identifiers for consistent rollout behavior:

```javascript
// Good - consistent experience
client.isEnabled('feature', { userId: user.id });

// Bad - random on each evaluation
client.isEnabled('feature');
```

### 3. Handle SSE Gracefully

SSE is optional - the SDK should work without it:

```javascript
class Client {
  constructor(options) {
    this.cache = new Map();

    // SSE enhances but isn't required
    if (options.realtime !== false) {
      this.connectSSE().catch(() => {
        console.warn('SSE unavailable, using polling fallback');
      });
    }
  }
}
```

### 4. Batch Telemetry

Don't send telemetry on every evaluation:

```javascript
class TelemetryBatcher {
  constructor() {
    this.queue = [];
    this.flushInterval = setInterval(() => this.flush(), 5000);
  }

  track(event) {
    this.queue.push(event);
    if (this.queue.length >= 100) {
      this.flush();
    }
  }

  flush() {
    if (this.queue.length === 0) return;
    const events = this.queue.splice(0);
    sendTelemetry(events);
  }
}
```

### 5. Clean Shutdown

Properly close connections on shutdown:

```javascript
class Client {
  destroy() {
    this.sseController?.abort();
    this.telemetryBatcher.flush();
    this.cache.clear();
  }
}

// Usage
process.on('SIGTERM', () => client.destroy());
```

---

## API Reference Summary

| Endpoint | Method | Auth | Purpose |
|----------|--------|------|---------|
| `/api/flags/{key}/evaluate` | POST | SDK/Server Key or JWT | Evaluate a flag |
| `/api/sdk/flags` | GET | SDK/Server Key | Get all flags for application + enterprise |
| `/api/sdk/enterprise-flags` | GET | SDK/Server Key | Get only enterprise-scoped flags |
| `/api/flags/stream` | GET | SDK/Server Key or JWT | Real-time SSE stream |
| `/api/telemetry/evaluations` | POST | SDK/Server Key | Report evaluations |
| `/api/telemetry/errors` | POST | SDK/Server Key | Report errors |

> **Note:** All endpoints require authentication. The evaluate and stream endpoints accept both SDK/Server keys and JWT tokens, allowing use from both SDKs and the admin UI. The `/api/sdk/*` endpoints are specifically designed for SDK consumption with local override support.

---

## Related Documentation

- [ADMIN-UI-DOGFOODING.md](./ADMIN-UI-DOGFOODING.md) - How the admin UI uses Otto Flags flags internally
- [PRD.md](./PRD.md) - Product requirements and feature overview
- External: [otto-flags](https://github.com/savvagent/otto-flags) - Official SDK implementations
