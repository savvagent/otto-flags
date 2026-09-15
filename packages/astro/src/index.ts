/**
 * @otto-flags/astro - Astro integration for Otto Flags feature flags
 *
 * Provides Astro integration, middleware, and server-side helpers.
 *
 * @packageDocumentation
 */

import type { AstroIntegration } from 'astro';
import { FlagClient, FlagClientConfig, FlagContext, FlagEvaluationResult } from '@otto-flags/sdk';

let clientInstance: FlagClient | null = null;

export interface OttoFlagsIntegrationOptions {
  /** Otto Flags configuration */
  config: FlagClientConfig;
}

/**
 * Astro integration for Otto Flags.
 * Automatically initializes the client and makes it available in all components.
 *
 * @param options - Integration options
 * @returns Astro integration
 *
 * @example
 * ```ts
 * // astro.config.mjs
 * import { defineConfig } from 'astro/config';
 * import ottoFlags from '@otto-flags/astro';
 *
 * export default defineConfig({
 *   integrations: [
 *     ottoFlags({
 *       config: {
 *         apiKey: process.env.OTTO_FLAGS_API_KEY,
 *         applicationId: process.env.OTTO_FLAGS_APP_ID,
 *       },
 *     }),
 *   ],
 * });
 * ```
 */
export default function ottoFlags(
  options: OttoFlagsIntegrationOptions
): AstroIntegration {
  return {
    name: '@otto-flags/astro',
    hooks: {
      'astro:config:setup': () => {
        // Initialize client
        if (!clientInstance) {
          clientInstance = new FlagClient(options.config);
        }
      },
      'astro:build:done': () => {
        // Cleanup
        if (clientInstance) {
          clientInstance.close();
          clientInstance = null;
        }
      },
    },
  };
}

/**
 * Initialize the Otto Flags client manually.
 * Use this if not using the Astro integration.
 *
 * @param config - Client configuration
 * @returns The FlagClient instance
 */
export function initOttoFlags(config: FlagClientConfig): FlagClient {
  if (!clientInstance) {
    clientInstance = new FlagClient(config);
  }
  return clientInstance;
}

/**
 * Get the Otto Flags client instance.
 *
 * @returns The FlagClient instance
 * @throws Error if client is not initialized
 */
export function getOttoFlags(): FlagClient {
  if (!clientInstance) {
    throw new Error(
      'Otto Flags client not initialized. Use the integration or call initOttoFlags() first.'
    );
  }
  return clientInstance;
}

/**
 * Extract context from Astro request.
 *
 * @param request - Astro Request
 * @param overrides - Additional context properties
 * @returns FlagContext
 */
export function getRequestContext(
  request: Request,
  overrides?: FlagContext
): FlagContext {
  const cookieHeader = request.headers.get('cookie') || '';
  const cookies = Object.fromEntries(
    cookieHeader.split('; ').map((c) => {
      const [key, ...values] = c.split('=');
      return [key, values.join('=')];
    })
  );

  const context: FlagContext = {
    user_id: cookies.user_id,
    anonymous_id: cookies.otto_flags_anonymous_id,
    session_id: cookies.session_id,
    language: request.headers.get('accept-language')?.split(',')[0] || undefined,
    ...overrides,
  };

  return context;
}

/**
 * Check if a feature flag is enabled.
 *
 * @param flagKey - The flag key to evaluate
 * @param context - Optional context for targeting
 * @returns Promise<boolean>
 *
 * @example
 * ```astro
 * ---
 * import { isEnabled } from '@otto-flags/astro';
 *
 * const showNewLayout = await isEnabled('new-layout');
 * ---
 *
 * {showNewLayout ? (
 *   <NewLayout />
 * ) : (
 *   <OldLayout />
 * )}
 * ```
 */
export async function isEnabled(
  flagKey: string,
  context?: FlagContext
): Promise<boolean> {
  const client = getOttoFlags();
  return client.isEnabled(flagKey, context);
}

/**
 * Evaluate a feature flag with detailed result.
 *
 * @param flagKey - The flag key to evaluate
 * @param context - Optional context for targeting
 * @returns Promise<FlagEvaluationResult>
 *
 * @example
 * ```astro
 * ---
 * import { evaluate } from '@otto-flags/astro';
 *
 * const result = await evaluate('premium-features');
 * const { value, reason, metadata } = result;
 * ---
 *
 * <div>
 *   <p>Enabled: {value}</p>
 *   <p>Reason: {reason}</p>
 * </div>
 * ```
 */
export async function evaluate(
  flagKey: string,
  context?: FlagContext
): Promise<FlagEvaluationResult> {
  const client = getOttoFlags();
  return client.evaluate(flagKey, context);
}

/**
 * Execute code conditionally based on flag value.
 *
 * @param flagKey - The flag key to check
 * @param callback - Function to execute if flag is enabled
 * @param context - Optional context for targeting
 * @returns Promise with callback result or null
 *
 * @example
 * ```astro
 * ---
 * import { withFlag } from '@otto-flags/astro';
 *
 * const data = await withFlag('use-new-api', async () => {
 *   return await fetchFromNewAPI();
 * });
 * ---
 *
 * {data ? <NewView data={data} /> : <OldView />}
 * ```
 */
export async function withFlag<T>(
  flagKey: string,
  callback: () => T | Promise<T>,
  context?: FlagContext
): Promise<T | null> {
  const client = getOttoFlags();
  return client.withFlag(flagKey, callback, context);
}

/**
 * Track an error with flag context.
 *
 * @param flagKey - The flag key associated with the error
 * @param error - The error that occurred
 * @param context - Optional context
 *
 * @example
 * ```astro
 * ---
 * import { trackError } from '@otto-flags/astro';
 *
 * try {
 *   await processData();
 * } catch (error) {
 *   trackError('data-processor', error);
 * }
 * ---
 * ```
 */
export function trackError(
  flagKey: string,
  error: Error,
  context?: FlagContext
): void {
  const client = getOttoFlags();
  client.trackError(flagKey, error, context);
}

/**
 * Set the environment for flag evaluation.
 * Useful for dynamically switching environments.
 *
 * @param environment - The environment name (e.g., "development", "staging", "production")
 *
 * @example
 * ```astro
 * ---
 * import { setEnvironment } from '@otto-flags/astro';
 *
 * setEnvironment('staging');
 * ---
 * ```
 */
export function setEnvironment(environment: string): void {
  const client = getOttoFlags();
  client.setEnvironment(environment);
}

/**
 * Get the current environment.
 *
 * @returns The current environment name
 *
 * @example
 * ```astro
 * ---
 * import { getEnvironment } from '@otto-flags/astro';
 *
 * const env = getEnvironment();
 * ---
 * <p>Environment: {env}</p>
 * ```
 */
export function getEnvironment(): string {
  const client = getOttoFlags();
  return client.getEnvironment();
}

/**
 * Helper to evaluate flags with automatic request context extraction.
 *
 * @param request - Astro Request
 * @param flagKey - The flag key to evaluate
 * @param context - Optional additional context
 * @returns Promise<boolean>
 *
 * @example
 * ```astro
 * ---
 * import { evaluateForRequest } from '@otto-flags/astro';
 *
 * const showBeta = await evaluateForRequest(Astro.request, 'beta-ui');
 * ---
 *
 * {showBeta ? <BetaUI /> : <StandardUI />}
 * ```
 */
export async function evaluateForRequest(
  request: Request,
  flagKey: string,
  context?: FlagContext
): Promise<boolean> {
  const requestContext = getRequestContext(request, context);
  return isEnabled(flagKey, requestContext);
}

/**
 * Create Astro middleware for feature flag-based routing.
 *
 * @example
 * ```ts
 * // src/middleware.ts
 * import { sequence } from 'astro/middleware';
 * import { createFlagMiddleware } from '@otto-flags/astro';
 *
 * const flagMiddleware = createFlagMiddleware({
 *   'maintenance-mode': {
 *     redirect: '/maintenance',
 *   },
 *   'beta-access': {
 *     rewrite: (url) => url.pathname.startsWith('/') ? '/beta' + url.pathname : url.pathname,
 *   },
 * });
 *
 * export const onRequest = sequence(flagMiddleware);
 * ```
 */
export interface FlagMiddlewareConfig {
  [flagKey: string]: {
    /** Redirect to this path if flag is enabled */
    redirect?: string;
    /** Rewrite URL if flag is enabled */
    rewrite?: (url: URL) => string;
    /** Custom handler if flag is enabled */
    handler?: (context: any) => Promise<Response | void>;
  };
}

export function createFlagMiddleware(config: FlagMiddlewareConfig) {
  return async (context: any, next: () => Promise<Response>) => {
    const client = getOttoFlags();
    const requestContext = getRequestContext(context.request);

    for (const [flagKey, options] of Object.entries(config)) {
      const enabled = await client.isEnabled(flagKey, requestContext);

      if (enabled) {
        if (options.redirect) {
          return Response.redirect(
            new URL(options.redirect, context.request.url),
            302
          );
        }

        if (options.rewrite) {
          const newPath = options.rewrite(new URL(context.request.url));
          context.url = new URL(newPath, context.request.url);
        }

        if (options.handler) {
          const response = await options.handler(context);
          if (response) return response;
        }
      }
    }

    return next();
  };
}

// Re-export types
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
