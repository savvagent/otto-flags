# Otto Flags Remix Example

Example Remix application demonstrating how to use the Otto Flags Remix SDK with loaders and actions.

## Features

- Remix with server-side rendering
- TypeScript
- Vite for fast development
- Otto Flags Remix loaders and utilities
- Server-side feature flag evaluation
- Client-side hydration

## Setup

1. **Install dependencies:**
   ```bash
   pnpm install
   ```

2. **Configure environment variables:**
   ```bash
   cp .env.example .env
   ```

   Edit `.env`:
   ```bash
   OTTO_FLAGS_API_URL=http://localhost:8080
   OTTO_FLAGS_SDK_KEY=your-sdk-key-here
   ```

3. **Run the development server:**
   ```bash
   pnpm dev
   ```

4. Open [http://localhost:5177](http://localhost:5177)

## Usage

### Using in Loaders

```typescript
import type { LoaderFunctionArgs } from '@remix-run/node';
import { json } from '@remix-run/node';
import { createOttoFlagsLoader } from '@otto-flags/remix';

const ottoFlags = createOttoFlagsLoader({
  apiUrl: process.env.OTTO_FLAGS_API_URL!,
  sdkKey: process.env.OTTO_FLAGS_SDK_KEY!,
});

export async function loader({ request }: LoaderFunctionArgs) {
  const isEnabled = await ottoFlags.isEnabled('new-feature', {
    userId: 'user-123',
    attributes: {
      email: 'user@example.com',
    },
  });

  return json({ isEnabled });
}
```

### Using in Components

```typescript
import { useLoaderData } from '@remix-run/react';

export default function Route() {
  const { isEnabled } = useLoaderData<typeof loader>();

  return isEnabled ? <NewFeature /> : <OldFeature />;
}
```

## Learn More

- [Remix Documentation](https://remix.run/docs)
- [Otto Flags Remix SDK Documentation](../../packages/remix/README.md)
- [SDK Integration Guide](../../docs/SDK-INTEGRATION.md)
