/**
 * @otto-flags/sveltekit - SvelteKit SDK for Otto Flags feature flags
 *
 * This package provides SvelteKit-specific integrations including:
 * - Server-side load functions and form actions
 * - Client-side stores from @otto-flags/svelte
 *
 * @packageDocumentation
 */

// Re-export client-side functionality from @otto-flags/svelte
export {
  initOttoFlags,
  getOttoFlags,
  createFlagStore,
  createFlag,
  createUserIdStore,
  trackError as trackErrorClient,
  setEnvironment,
  getEnvironment,
} from '@otto-flags/svelte';

export type {
  FlagStoreOptions,
  FlagStoreValue,
} from '@otto-flags/svelte';

// Re-export types from core SDK
export type {
  FlagClientConfig,
  FlagContext,
  FlagEvaluationResult,
  EvaluationEvent,
  ErrorEvent,
  FlagUpdateEvent,
  // Generated API types for advanced users
  ApiTypes,
  components,
} from '@otto-flags/sdk';

// Re-export FlagClient for advanced use cases
export { FlagClient } from '@otto-flags/sdk';
