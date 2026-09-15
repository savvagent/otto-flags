# @otto-flags/vue

Vue 3 SDK for Otto Flags with Composition API composables.

## Installation

```bash
npm install @otto-flags/vue
```

## Quick Start

```vue
<script setup>
import { createApp } from 'vue';
import { OttoFlagsPlugin, useFlag } from '@otto-flags/vue';
import App from './App.vue';

// Install plugin
const app = createApp(App);
app.use(OttoFlagsPlugin, {
  apiKey: 'sdk_...',
  applicationId: 'your-app-id',
});
app.mount('#app');
</script>
```

## Composables

### `useFlag(flagKey, options)`

```vue
<script setup>
import { useFlag } from '@otto-flags/vue';

const { value: isEnabled, loading } = useFlag('new-feature', {
  context: { user_id: user.value?.id },
  defaultValue: false,
  realtime: true,
});
</script>

<template>
  <div v-if="loading">Loading...</div>
  <NewFeature v-else-if="isEnabled" />
  <OldFeature v-else />
</template>
```

### `useOttoFlags()`

Get the client instance for advanced usage.

### `useUser()`

Manage user identification.

```vue
<script setup>
import { useUser } from '@otto-flags/vue';
import { watch } from 'vue';

const { setUserId } = useUser();

watch(currentUser, (user) => {
  setUserId(user?.id || null);
});
</script>
```

### `useTrackError(flagKey, context)`

Track errors with flag context.

## License

MIT
