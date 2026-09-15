# Otto Flags React Example

Example React application demonstrating how to use the Otto Flags React SDK with hooks.

## Features

- React 18 with Vite
- TypeScript
- Otto Flags React hooks (`useFeatureFlag`, `useOttoFlags`)
- Real-time feature flag updates
- Hot module replacement

## Setup

1. **Install dependencies:**
   ```bash
   pnpm install
   ```

2. **Configure environment variables:**
   ```bash
   cp .env.example .env.local
   ```

   Edit `.env.local`:
   ```bash
   VITE_OTTO_FLAGS_API_URL=http://localhost:8080
   VITE_OTTO_FLAGS_SDK_KEY=your-sdk-key-here
   ```

3. **Run the development server:**
   ```bash
   pnpm dev
   ```

4. Open [http://localhost:5173](http://localhost:5173)

## Usage

### Using the React Hook

```typescript
import { useFeatureFlag } from '@otto-flags/react';

function MyComponent() {
  const { isEnabled, loading } = useFeatureFlag('new-feature', {
    userId: 'user-123',
    attributes: {
      email: 'user@example.com',
      plan: 'pro',
    },
  });

  if (loading) return <div>Loading...</div>;

  return isEnabled ? <NewFeature /> : <OldFeature />;
}
```

### Using the Provider

```typescript
import { OttoFlagsProvider } from '@otto-flags/react';

function App() {
  return (
    <OttoFlagsProvider
      apiUrl={import.meta.env.VITE_OTTO_FLAGS_API_URL}
      sdkKey={import.meta.env.VITE_OTTO_FLAGS_SDK_KEY}
    >
      <MyApp />
    </OttoFlagsProvider>
  );
}
```

## Learn More

- [React Documentation](https://react.dev)
- [Otto Flags React SDK Documentation](../../packages/react/README.md)
- [SDK Integration Guide](../../docs/SDK-INTEGRATION.md)
