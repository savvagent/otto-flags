# Migration Guide

This guide helps you migrate from using the SDKs in the main `savvagent-flags` repository to the new `otto-flags` repository.

## Overview

The SDKs, MCP servers, and example applications have been moved to a dedicated repository with the following benefits:

- Independent versioning for each package
- Simplified publishing workflow
- Better documentation focused on SDK usage
- Clearer separation between platform and client code
- Easier community contributions

## Package Name Changes

| Old Package Name | New Package Name |
|-----------------|------------------|
| `@otto-flags/typescript` | `@otto-flags/sdk` |
| `@otto-flags/mcp-sdk` | `@otto-flags/mcp-sdk` (unchanged) |
| `@otto-flags/mcp-sentry` | `@otto-flags/mcp-sentry` (unchanged) |

## For Application Developers

### Step 1: Update package.json

**Before:**
```json
{
  "dependencies": {
    "@otto-flags/typescript": "file:../savvagent-flags/sdks/typescript"
  }
}
```

**After (published packages):**
```json
{
  "dependencies": {
    "@otto-flags/sdk": "^0.1.0"
  }
}
```

### Step 2: Update imports

The package name has changed from `@otto-flags/typescript` to `@otto-flags/sdk`:

**Before:**
```typescript
import { OttoFlagsClient } from '@otto-flags/typescript';
```

**After:**
```typescript
import { OttoFlagsClient } from '@otto-flags/sdk';
```

### Step 3: Install dependencies

```bash
# Remove old package
pnpm remove @otto-flags/typescript

# Install new package
pnpm add @otto-flags/sdk
```

## For SDK Contributors

### Local Development Setup

1. **Clone both repositories:**
```bash
cd ~/dev
git clone https://github.com/yourusername/savvagent-flags.git
git clone https://github.com/yourusername/otto-flags.git
```

2. **Install dependencies:**
```bash
cd otto-flags
pnpm install
```

3. **Build packages:**
```bash
pnpm build
```

### Using Local SDK in Platform Development

To test SDK changes with the platform, use pnpm's `file:` protocol:

**In savvagent-flags/frontend/package.json:**
```json
{
  "dependencies": {
    "@otto-flags/sdk": "file:../../otto-flags/packages/typescript"
  }
}
```

Then run:
```bash
cd savvagent-flags/frontend
pnpm install
```

Changes to the SDK will be reflected immediately without needing to rebuild (when using source files).

### Making Changes

1. **Create a branch:**
```bash
git checkout -b feature/my-feature
```

2. **Make your changes to the SDK**

3. **Add a changeset:**
```bash
pnpm changeset
```

Follow the prompts to:
- Select which package changed
- Choose semver bump type (major/minor/patch)
- Write a changelog entry

4. **Commit and push:**
```bash
git add .
git commit -m "feat: add new feature"
git push origin feature/my-feature
```

5. **Create a Pull Request**

When the PR is merged, a GitHub Action will:
- Create a "Version Packages" PR with version bumps
- Once that PR is merged, automatically publish to npm

## For MCP Server Users

MCP server packages remain unchanged:

```bash
pnpm add @otto-flags/mcp-sdk
pnpm add @otto-flags/mcp-sentry
```

## Repository Structure

```
otto-flags/
├── packages/
│   ├── typescript/       # @otto-flags/sdk
│   ├── mcp-sdk/          # @otto-flags/mcp-sdk
│   └── mcp-sentry/       # @otto-flags/mcp-sentry
├── examples/
│   ├── nextjs-app/       # Next.js example
│   ├── sveltekit-app/    # SvelteKit example
│   └── node-backend/     # Node.js backend example
└── docs/
    ├── MIGRATION.md      # This file
    └── SDK-INTEGRATION.md
```

## Breaking Changes

### Version 0.1.0 → 0.2.0

- Package renamed from `@otto-flags/typescript` to `@otto-flags/sdk`
- No API changes; only import path changed

## Troubleshooting

### "Cannot find module '@otto-flags/typescript'"

You're still importing the old package name. Update to `@otto-flags/sdk`:

```typescript
// Old
import { OttoFlagsClient } from '@otto-flags/typescript';

// New
import { OttoFlagsClient } from '@otto-flags/sdk';
```

### "Module not found" after migration

Clear your dependency cache and reinstall:

```bash
rm -rf node_modules pnpm-lock.yaml
pnpm install
```

### Local development not working

Ensure you're using the `file:` protocol with the correct relative path:

```json
{
  "dependencies": {
    "@otto-flags/sdk": "file:../../otto-flags/packages/typescript"
  }
}
```

Then run `pnpm install` to create the symlink.

## Related Documentation

- [SDK-DEVELOPER-GUIDE.md](./SDK-DEVELOPER-GUIDE.md) - Official API specification
- [SDK-INTEGRATION.md](./SDK-INTEGRATION.md) - SDK integration guide

## Support

- GitHub Issues: https://github.com/savvagent/otto-flags/issues
- Documentation: https://github.com/savvagent/otto-flags/tree/main/docs
- Examples: https://github.com/savvagent/otto-flags/tree/main/examples
