# @otto-flags/sveltekit

SvelteKit SDK for Otto Flags with server-side load functions and client-side stores.

## Installation

```bash
npm install @otto-flags/sveltekit
```

## Quick Start

### Server-Side (Load Functions)

```ts
// src/hooks.server.ts
import { initSvelteKitServer } from '@otto-flags/sveltekit/server';

initSvelteKitServer({
  apiKey: process.env.OTTO_FLAGS_API_KEY!,
});
```

```ts
// +page.server.ts
import { isEnabled } from '@otto-flags/sveltekit/server';

export async function load({ cookies }) {
  const enabled = await isEnabled('new-feature', {
    user_id: cookies.get('user_id'),
  });

  return { enabled };
}
```

### Client-Side (Stores)

```svelte
<!-- +layout.svelte -->
<script>
import { initOttoFlags } from '@otto-flags/sveltekit';

initOttoFlags({
  apiKey: import.meta.env.VITE_OTTO_FLAGS_API_KEY,
});
</script>
```

```svelte
<!-- +page.svelte -->
<script>
import { createFlag } from '@otto-flags/sveltekit';

const isEnabled = createFlag('client-feature');
</script>

{#if $isEnabled}
  <NewFeature />
{/if}
```

## API Reference

### Server-Side (`@otto-flags/sveltekit/server`)

- `initSvelteKitServer(config)` - Initialize server client
- `isEnabled(flagKey, context?)` - Check if flag is enabled
- `evaluate(flagKey, context?)` - Get detailed result
- `evaluateForEvent(event, flagKey, context?)` - Evaluate with event context
- `getEventContext(event, overrides?)` - Extract context from event
- `trackError(flagKey, error, context?)` - Track errors

### Client-Side

All stores and functions from `@otto-flags/svelte` are available.

## License

MIT
