# Otto Flags

otto-flags is a feature flag platform built for agentic coding agents to manage, not
for humans to click through. **See [VISION.md](./VISION.md) for the full scope and
vision** — the short version: flag *management* (create, target, roll out, roll back,
assess risk) is an MCP tool surface an agent calls; flag *evaluation* inside a running
production app stays SDK/REST, because a prod request can't do an LLM tool-call round
trip.

## Using it

**Agents** connect an MCP client to `https://otto-flags.savvagent.com/mcp`. There is
nothing to install: the client is sent to the otto platform to sign in (one account
covers every otto service), then calls tools such as `create_flag`,
`set_flag_environment`, `flag_health`, and `rollback_flag`. Call `whoami` first.

**Running apps** evaluate flags through the SDKs below, pointed at
`https://otto-flags.savvagent.com` with the app's key: the public client key (`sdk_…`)
in browsers and mobile apps, the secret server key (`srv_…`) on servers. An agent
creates the app and its keys with `create_app`. The REST contract is
[docs/SDK-DEVELOPER-GUIDE.md](./docs/SDK-DEVELOPER-GUIDE.md).

## The server

A Rust workspace under `crates/`, and a resource server of the otto platform, which
owns identity, OAuth, orgs, and billing:

| Crate | What |
|---|---|
| `flags-core` | Schema, flags and their version history, the evaluation engine, telemetry |
| `flags-mcp` | The MCP tools, behind the platform's OAuth |
| `flags-api` | The SDK REST API and the platform's webhooks |
| `flags-server` | The binary: config, router, background tasks |

```bash
podman compose up -d                     # Postgres on :15434
cp .env.example .env                     # then fill in the platform settings
DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace
cargo run -p flags-server
```

Deployment: [docs/deploy/fly.md](./docs/deploy/fly.md). How v1 was planned and what was
deferred: [docs/plans/2026-10-09-build-and-deploy.md](./docs/plans/2026-10-09-build-and-deploy.md).

## Packages

The evaluation-side SDKs, the example apps, and the observability MCP clients
(Sentry, Datadog, …) that future incident-correlation tools will use.

### Client SDKs

- **[@otto-flags/sdk](./packages/typescript)** - TypeScript/JavaScript SDK for feature flags
  - Works with React, Next.js, SvelteKit, Node.js, and more
  - Real-time flag updates via WebSocket
  - Built-in caching and telemetry
  - Type-safe API

### Framework SDKs

- **[@otto-flags/react](./packages/react)** - React hooks for feature flags
- **[@otto-flags/vue](./packages/vue)** - Vue 3 composables for feature flags
- **[@otto-flags/solid](./packages/solid)** - SolidJS primitives for feature flags
- **[@otto-flags/svelte](./packages/svelte)** - Svelte stores for feature flags
- **[@otto-flags/nextjs](./packages/nextjs)** - Next.js integration with App Router & Pages Router
- **[@otto-flags/remix](./packages/remix)** - Remix loaders and actions integration
- **[@otto-flags/sveltekit](./packages/sveltekit)** - SvelteKit server-side integration
- **[@otto-flags/astro](./packages/astro)** - Astro integration for feature flags

### Mobile SDKs

- **[OttoFlagsSDK (iOS)](./packages/ios-sdk)** - iOS SDK
  - Native Swift with async/await
  - SwiftUI and UIKit support
  - Real-time updates via WebSocket
  - Works on iOS, macOS, tvOS, and watchOS

- **[otto-flags-android-sdk](./packages/android-sdk)** - Android SDK
  - Native Kotlin with Coroutines
  - Jetpack Compose integration
  - Flow-based reactive updates
  - Material Design 3 support

### Server SDKs

- **[@otto-flags/node-server](./packages/node-server)** - Node.js Server SDK
  - Built for Express, Fastify, NestJS, and more
  - Server-Sent Events for real-time updates
  - In-memory caching with configurable TTL
  - Full TypeScript support

- **[otto-flags-java-server-sdk](./packages/java-server)** - Java Server SDK
  - Maven and Gradle support
  - Thread-safe concurrent access
  - OkHttp-based HTTP client
  - Comprehensive JavaDocs

- **[otto-flags-go-server-sdk](./packages/go-server)** - Go Server SDK
  - Goroutine-safe concurrent access
  - Idiomatic Go patterns
  - Minimal dependencies
  - High-performance caching

- **[otto-flags (Rust)](./packages/rust-server)** - Rust Server SDK
  - Async/await with Tokio runtime
  - Zero-cost abstractions
  - Memory-safe with Rust ownership
  - Full type safety

### MCP Servers

- **[@otto-flags/mcp-sdk](./packages/mcp-sdk)** - Base SDK for building MCP integrations
- **[@otto-flags/mcp-sentry](./packages/mcp-sentry)** - Sentry error tracking integration

## Quick Start

### Installation

```bash
# Using pnpm (recommended)
pnpm add @otto-flags/sdk

# Using npm
npm install @otto-flags/sdk

# Using yarn
yarn add @otto-flags/sdk
```

### Basic Usage

```typescript
import { OttoFlagsClient } from '@otto-flags/sdk';

const client = new OttoFlagsClient({
  apiUrl: 'https://otto-flags.savvagent.com',
  sdkKey: 'your-sdk-key',
  environment: 'production',
});

// Check if a feature is enabled
const isEnabled = await client.isEnabled('new-feature', {
  userId: 'user-123',
  attributes: {
    email: 'user@example.com',
    plan: 'pro',
  },
});

if (isEnabled) {
  // Show new feature
} else {
  // Show old feature
}
```

## Examples

Working example applications are available in the [examples](./examples) directory:

### Mobile Examples

- **[iOS App](./examples/ios-app)** - SwiftUI app with native iOS SDK
- **[Android App](./examples/android-app)** - Jetpack Compose app with Kotlin

### Client & Framework Examples

- **[React App](./examples/react-app)** - React 18 with Vite and hooks
- **[Vue App](./examples/vue-app)** - Vue 3 with Composition API
- **[Solid App](./examples/solid-app)** - SolidJS with reactive primitives
- **[Svelte App](./examples/svelte-app)** - Svelte 4 with stores
- **[Next.js App](./examples/nextjs-app)** - React Server Components + Client Components
- **[Remix App](./examples/remix-app)** - Remix with loaders and server-side evaluation
- **[SvelteKit App](./examples/sveltekit-app)** - Svelte 5 with runes
- **[Astro App](./examples/astro-app)** - Astro with integration

### Server Examples

- **[Node.js Backend](./examples/node-backend)** - Express API server
- **[Java Server](./examples/java-server)** - Spring Boot 3.2 with Maven
- **[Go Server](./examples/go-server)** - Go with Gin framework
- **[Rust Server](./examples/rust-server)** - Rust with Axum and Tokio

## Documentation

- **[SDK Integration Guide](./docs/SDK-INTEGRATION.md)** - Complete integration instructions
- **[Migration Guide](./docs/MIGRATION.md)** - Migrating from the old repo structure
- **[API Reference](./packages/typescript/README.md)** - Full SDK API documentation
- **[MCP Integration Guide](./packages/mcp-sdk/README.md)** - Building MCP servers

## AI-Assisted Development

Use AI coding assistants (Claude Code, Cursor, GitHub Copilot) to integrate Otto Flags faster. We provide AI-optimized documentation:

| File | Description | Use Case |
|------|-------------|----------|
| [llms.txt](./llms.txt) | Quick reference (~4KB) | Fast lookups, links to detailed docs |
| [llms-full.txt](./llms-full.txt) | Complete docs (~15KB) | Full context for complex integrations |

### Quick Start with AI

**Claude Code / Cursor:**
```
Add Otto Flags feature flags to my React app.
Reference: https://raw.githubusercontent.com/ottoFlags/otto-flags/main/llms-full.txt
```

**Add to project context** for better suggestions:
```bash
# Claude Code - add to .claude/settings.json
# Cursor - add to Settings > Features > Docs
https://raw.githubusercontent.com/ottoFlags/otto-flags/main/llms-full.txt
```

See **[AI-Assisted Development Guide](./docs/AI-ASSISTED-DEVELOPMENT.md)** for detailed instructions.

## Development

This is a pnpm workspace monorepo with independent package versioning.

### Prerequisites

- Node.js 18+
- pnpm 8+

### Setup

```bash
# Clone the repository
git clone https://github.com/yourusername/otto-flags.git
cd otto-flags

# Install dependencies
pnpm install

# Build all packages
pnpm build

# Run tests
pnpm test

# Lint code
pnpm lint
```

### Local Development

To use local packages in another project:

```json
{
  "dependencies": {
    "@otto-flags/sdk": "file:../otto-flags/packages/typescript"
  }
}
```

### Making Changes

1. Create a branch for your changes
2. Make your changes
3. Add a changeset:
   ```bash
   pnpm changeset
   ```
4. Commit and push your changes
5. Create a pull request

When your PR is merged:
- A "Version Packages" PR will be created automatically
- Merging that PR will publish to npm

### Project Structure

```
otto-flags/
├── packages/
│   ├── typescript/          # @otto-flags/sdk (base TypeScript SDK)
│   ├── react/              # @otto-flags/react
│   ├── vue/                # @otto-flags/vue
│   ├── solid/              # @otto-flags/solid
│   ├── svelte/             # @otto-flags/svelte
│   ├── nextjs/             # @otto-flags/nextjs
│   ├── remix/              # @otto-flags/remix
│   ├── sveltekit/          # @otto-flags/sveltekit
│   ├── astro/              # @otto-flags/astro
│   ├── ios-sdk/            # iOS SDK (Swift)
│   ├── android-sdk/        # Android SDK (Kotlin)
│   ├── node-server/        # @otto-flags/node-server
│   ├── java-server/        # Java server SDK
│   ├── go-server/          # Go server SDK
│   ├── rust-server/        # Rust server SDK
│   ├── mcp-sdk/            # @otto-flags/mcp-sdk
│   └── mcp-sentry/         # @otto-flags/mcp-sentry
├── examples/
│   ├── ios-app/            # iOS SwiftUI example
│   ├── android-app/        # Android Jetpack Compose example
│   ├── react-app/          # React example
│   ├── vue-app/            # Vue example
│   ├── solid-app/          # SolidJS example
│   ├── svelte-app/         # Svelte example
│   ├── nextjs-app/         # Next.js example
│   ├── remix-app/          # Remix example
│   ├── sveltekit-app/      # SvelteKit example
│   ├── astro-app/          # Astro example
│   ├── node-backend/       # Node.js example
│   ├── java-server/        # Java server example
│   ├── go-server/          # Go server example
│   └── rust-server/        # Rust server example
├── docs/                   # Documentation
├── .changeset/             # Changesets for versioning
└── .github/workflows/      # CI/CD pipelines
```

## Versioning

This monorepo uses [Changesets](https://github.com/changesets/changesets) for independent package versioning. Each package can have its own version number and be published independently.

### Semantic Versioning

- **Major (x.0.0)**: Breaking changes
- **Minor (0.x.0)**: New features (backwards compatible)
- **Patch (0.0.x)**: Bug fixes

## CI/CD

### Continuous Integration

All pull requests run:
- Linting
- Type checking
- Unit tests
- Build verification

### Automated Publishing

On merge to `main`:
1. Changesets creates a "Version Packages" PR
2. Merging that PR triggers npm publish
3. Git tags are created for each published version

## Contributing

We welcome contributions! Please see our contributing guidelines:

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Add tests if applicable
5. Run `pnpm changeset` to document changes
6. Submit a pull request

### Code Style

- TypeScript for all code
- ESLint + Prettier for formatting
- Conventional commits for messages

### Testing

```bash
# Run all tests
pnpm test

# Run tests for a specific package
pnpm --filter @otto-flags/sdk test

# Watch mode
pnpm --filter @otto-flags/sdk test:watch
```

## Support

- **Documentation**: [docs/](./docs)
- **Examples**: [examples/](./examples)
- **Issues**: [GitHub Issues](https://github.com/savvagent/otto-flags/issues)
- **Discussions**: [GitHub Discussions](https://github.com/savvagent/otto-flags/discussions)

## Related Projects

- **[Otto Flags Platform](https://github.com/savvagent/otto-flags)** - The main platform repository
- **[Otto Flags Docs](https://github.com/savvagent/otto-flags#readme)** - Official documentation

## Packages Status

### Mobile SDKs

| Package | Platform | Version | Status |
|---------|----------|---------|--------|
| [OttoFlagsSDK](./packages/ios-sdk) | iOS 13+ | v0.1.0 | Beta |
| [otto-flags-android-sdk](./packages/android-sdk) | Android 5.0+ | v0.1.0 | Beta |

### Client & Framework SDKs

| Package | Version | Status |
|---------|---------|--------|
| [@otto-flags/sdk](./packages/typescript) | [![npm](https://img.shields.io/npm/v/@otto-flags/sdk)](https://www.npmjs.com/package/@otto-flags/sdk) | Stable |
| [@otto-flags/react](./packages/react) | [![npm](https://img.shields.io/npm/v/@otto-flags/react)](https://www.npmjs.com/package/@otto-flags/react) | Beta |
| [@otto-flags/vue](./packages/vue) | [![npm](https://img.shields.io/npm/v/@otto-flags/vue)](https://www.npmjs.com/package/@otto-flags/vue) | Beta |
| [@otto-flags/solid](./packages/solid) | [![npm](https://img.shields.io/npm/v/@otto-flags/solid)](https://www.npmjs.com/package/@otto-flags/solid) | Beta |
| [@otto-flags/svelte](./packages/svelte) | [![npm](https://img.shields.io/npm/v/@otto-flags/svelte)](https://www.npmjs.com/package/@otto-flags/svelte) | Beta |
| [@otto-flags/nextjs](./packages/nextjs) | [![npm](https://img.shields.io/npm/v/@otto-flags/nextjs)](https://www.npmjs.com/package/@otto-flags/nextjs) | Beta |
| [@otto-flags/remix](./packages/remix) | [![npm](https://img.shields.io/npm/v/@otto-flags/remix)](https://www.npmjs.com/package/@otto-flags/remix) | Beta |
| [@otto-flags/sveltekit](./packages/sveltekit) | [![npm](https://img.shields.io/npm/v/@otto-flags/sveltekit)](https://www.npmjs.com/package/@otto-flags/sveltekit) | Beta |
| [@otto-flags/astro](./packages/astro) | [![npm](https://img.shields.io/npm/v/@otto-flags/astro)](https://www.npmjs.com/package/@otto-flags/astro) | Beta |

### Server SDKs

| Package | Language | Version | Status |
|---------|----------|---------|--------|
| [@otto-flags/node-server](./packages/node-server) | Node.js | v0.1.0 | Beta |
| [otto-flags-java-server-sdk](./packages/java-server) | Java 11+ | v0.1.0 | Beta |
| [otto-flags-go-server-sdk](./packages/go-server) | Go 1.21+ | v0.1.0 | Beta |
| [ottoFlags](./packages/rust-server) | Rust 1.70+ | v0.1.0 | Beta |

### MCP Servers

| Package | Version | Status |
|---------|---------|--------|
| [@otto-flags/mcp-sdk](./packages/mcp-sdk) | [![npm](https://img.shields.io/npm/v/@otto-flags/mcp-sdk)](https://www.npmjs.com/package/@otto-flags/mcp-sdk) | Beta |
| [@otto-flags/mcp-sentry](./packages/mcp-sentry) | [![npm](https://img.shields.io/npm/v/@otto-flags/mcp-sentry)](https://www.npmjs.com/package/@otto-flags/mcp-sentry) | Beta |

## License

Two licenses, by directory:

- **The server** (`crates/`) is [AGPL-3.0-or-later](./LICENSE) (see [NOTICE](./NOTICE)). Running a modified
  version as a network service obliges you to offer its source to its users.
- **The SDKs and examples** (`packages/`, `examples/`) are [MIT](./packages/typescript/LICENSE).
  Embedding an SDK in your application puts no AGPL obligations on it.

