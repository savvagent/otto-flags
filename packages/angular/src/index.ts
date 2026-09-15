/**
 * @otto-flags/angular - Angular SDK for Otto Flags feature flags
 *
 * This package provides Angular services and modules for easy integration
 * of Otto Flags feature flags into Angular applications.
 *
 * @packageDocumentation
 */

// Module
export { OttoFlagsModule } from './module';

// Service and types
export { OttoFlagsService, OTTO_FLAGS_CONFIG } from './service';
export type {
  OttoFlagsConfig,
  DefaultFlagContext,
  FlagObservableResult,
  FlagOptions,
} from './service';

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
