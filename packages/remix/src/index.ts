/**
 * @otto-flags/remix - Remix SDK for Otto Flags feature flags
 *
 * This package provides Remix-specific integrations including:
 * - Loader and action helpers
 * - Client-side hooks from @otto-flags/react
 * - Server-side flag evaluation
 *
 * @packageDocumentation
 */

// Server-side exports (for loaders and actions)
export {
  initRemixClient,
  getRemixClient,
  getRequestContext,
  isEnabled,
  evaluate,
  withFlag,
  trackError,
  evaluateForRequest,
  setEnvironment,
  getEnvironment,
} from './server';

// Client-side exports (re-export from React SDK)
export {
  OttoFlagsProvider,
  useOttoFlags,
  useFlag,
  useFlags,
  useWithFlag,
  useUser,
  useTrackError,
  useEnvironment,
} from '@otto-flags/react';

export type {
  OttoFlagsProviderProps,
  DefaultFlagContext,
  UseFlagOptions,
  UseFlagResult,
  UseFlagsOptions,
  UseFlagsResult,
} from '@otto-flags/react';

// Re-export types from core SDK
export type {
  FlagClientConfig,
  FlagContext,
  FlagEvaluationResult,
  EvaluationEvent,
  ErrorEvent,
  FlagUpdateEvent,
  FlagDefinition,
  FlagListResponse,
  // Generated API types for advanced users
  ApiTypes,
  components,
} from '@otto-flags/sdk';

// Re-export FlagClient for advanced use cases
export { FlagClient } from '@otto-flags/sdk';
