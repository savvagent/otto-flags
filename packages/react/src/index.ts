/**
 * @otto-flags/react - React SDK for Otto Flags feature flags
 *
 * This package provides React hooks and components for easy integration
 * of Otto Flags feature flags into React applications.
 *
 * @packageDocumentation
 */

// Context and Provider
export { OttoFlagsProvider, useOttoFlags } from './context';
export type { OttoFlagsProviderProps, DefaultFlagContext } from './context';

// Hooks
export {
  useFlag,
  useFlags,
  useWithFlag,
  useUser,
  useTrackError,
  useEnvironment,
} from './hooks';
export type {
  UseFlagOptions,
  UseFlagResult,
  UseFlagsOptions,
  UseFlagsResult,
} from './hooks';

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
