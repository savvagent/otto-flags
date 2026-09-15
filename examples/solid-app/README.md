# Otto Flags SolidJS Example

Example SolidJS application demonstrating how to use the Otto Flags Solid SDK with reactive primitives.

## Features

- SolidJS with fine-grained reactivity
- TypeScript
- Vite for fast development
- Otto Flags Solid primitives (`createFeatureFlag`, `OttoFlagsProvider`)
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

4. Open [http://localhost:5175](http://localhost:5175)

## Usage

### Using the Solid Primitive

```typescript
import { createFeatureFlag } from '@otto-flags/solid';

function MyComponent() {
  const [isEnabled, { loading }] = createFeatureFlag('new-feature', {
    userId: 'user-123',
    attributes: {
      email: 'user@example.com',
      plan: 'pro',
    },
  });

  return (
    <Show when={!loading()} fallback={<div>Loading...</div>}>
      <Show when={isEnabled()} fallback={<OldFeature />}>
        <NewFeature />
      </Show>
    </Show>
  );
}
```

### Using the Provider

```typescript
import { render } from 'solid-js/web';
import { OttoFlagsProvider } from '@otto-flags/solid';
import App from './App';

render(
  () => (
    <OttoFlagsProvider
      apiUrl={import.meta.env.VITE_OTTO_FLAGS_API_URL}
      sdkKey={import.meta.env.VITE_OTTO_FLAGS_SDK_KEY}
    >
      <App />
    </OttoFlagsProvider>
  ),
  document.getElementById('root')!
);
```

## Learn More

- [SolidJS Documentation](https://www.solidjs.com/)
- [Otto Flags Solid SDK Documentation](../../packages/solid/README.md)
- [SDK Integration Guide](../../docs/SDK-INTEGRATION.md)
