# @otto-flags/solid

SolidJS SDK for Otto Flags with reactive primitives (signals and resources).

## Installation

```bash
npm install @otto-flags/solid
```

## Quick Start

```tsx
import { OttoFlagsProvider, createFlag } from '@otto-flags/solid';
import { Show } from 'solid-js';

function App() {
  return (
    <OttoFlagsProvider config={{ apiKey: 'sdk_...' }}>
      <MyFeature />
    </OttoFlagsProvider>
  );
}

function MyFeature() {
  const flag = createFlag('new-feature');

  return (
    <Show when={!flag.loading()} fallback={<div>Loading...</div>}>
      <Show when={flag.value()} fallback={<OldFeature />}>
        <NewFeature />
      </Show>
    </Show>
  );
}
```

## API Reference

### `createFlag(flagKey, options)`

Create a reactive flag with full state.

```tsx
const flag = createFlag('new-feature', {
  context: { user_id: userId() },
  defaultValue: false,
  realtime: true,
});

// Access values
flag.value();   // boolean
flag.loading(); // boolean
flag.error();   // Error | null
flag.refetch(); // force re-evaluation
```

### `createFlagValue(flagKey, options)`

Simple accessor for flag value only.

```tsx
const isEnabled = createFlagValue('new-feature');

return <Show when={isEnabled()}><NewFeature /></Show>;
```

### `createUserSignals()`

Manage user identification.

```tsx
const [userId, setUserId] = createUserSignals();

createEffect(() => {
  setUserId(currentUser()?.id || null);
});
```

## License

MIT
