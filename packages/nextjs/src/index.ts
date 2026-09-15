/**
 * @otto-flags/nextjs - Next.js SDK for Otto Flags feature flags
 *
 * This package provides Next.js-specific integrations including:
 * - Client Components (hooks and provider)
 * - Server Components (async flag evaluation)
 * - Middleware (edge runtime support)
 * - Route Handlers and Server Actions
 *
 * @packageDocumentation
 */

/**
 * For Client Components, import from '@otto-flags/nextjs/client':
 *
 * ```tsx
 * 'use client';
 * import { useFlag, OttoFlagsProvider } from '@otto-flags/nextjs/client';
 * ```
 *
 * For Server Components, import from '@otto-flags/nextjs/server':
 *
 * ```tsx
 * import { isEnabled, evaluate } from '@otto-flags/nextjs/server';
 * ```
 *
 * For Middleware, import from '@otto-flags/nextjs/middleware':
 *
 * ```tsx
 * import { createMiddleware } from '@otto-flags/nextjs/middleware';
 * ```
 */

// Default exports (server-side by default for App Router)
export {
  initServerClient,
  getServerClient,
  createServerContext,
  isEnabled,
  evaluate,
  withFlag,
  trackError,
  evaluateForRequest,
  evaluateMultiple,
  isEnabledMultiple,
} from './server';

// Export server-specific types
export type {
  EvaluateMultipleResult,
  EvaluateMultipleOptions,
} from './server';

// Re-export types from core SDK
export type {
  FlagClientConfig,
  FlagContext,
  FlagEvaluationResult,
  FlagDefinition,
  FlagListResponse,
  EvaluationEvent,
  ErrorEvent,
  FlagUpdateEvent,
  // Generated API types for advanced users
  ApiTypes,
  components,
} from '@otto-flags/sdk';

// Re-export FlagClient for advanced use cases
export { FlagClient } from '@otto-flags/sdk';
