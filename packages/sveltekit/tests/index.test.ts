/**
 * Tests for @otto-flags/sveltekit client-side exports
 *
 * This test suite verifies:
 * - Re-exports from @otto-flags/svelte are properly exposed
 * - Type exports are available
 * - FlagClient is re-exported from core SDK
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';

describe('@otto-flags/sveltekit - Client-side exports', () => {
  describe('Module re-exports', () => {
    it('should re-export initOttoFlags from @otto-flags/svelte', async () => {
      const { initOttoFlags } = await import('../src/index');
      expect(initOttoFlags).toBeDefined();
      expect(typeof initOttoFlags).toBe('function');
    });

    it('should re-export getOttoFlags from @otto-flags/svelte', async () => {
      const { getOttoFlags } = await import('../src/index');
      expect(getOttoFlags).toBeDefined();
      expect(typeof getOttoFlags).toBe('function');
    });

    it('should re-export createFlagStore from @otto-flags/svelte', async () => {
      const { createFlagStore } = await import('../src/index');
      expect(createFlagStore).toBeDefined();
      expect(typeof createFlagStore).toBe('function');
    });

    it('should re-export createFlag from @otto-flags/svelte', async () => {
      const { createFlag } = await import('../src/index');
      expect(createFlag).toBeDefined();
      expect(typeof createFlag).toBe('function');
    });

    it('should re-export createUserIdStore from @otto-flags/svelte', async () => {
      const { createUserIdStore } = await import('../src/index');
      expect(createUserIdStore).toBeDefined();
      expect(typeof createUserIdStore).toBe('function');
    });

    it('should re-export trackErrorClient as trackErrorClient from @otto-flags/svelte', async () => {
      const { trackErrorClient } = await import('../src/index');
      expect(trackErrorClient).toBeDefined();
      expect(typeof trackErrorClient).toBe('function');
    });

    it('should re-export FlagClient from @otto-flags/sdk', async () => {
      const { FlagClient } = await import('../src/index');
      expect(FlagClient).toBeDefined();
      expect(typeof FlagClient).toBe('function');
    });
  });

  describe('Type exports', () => {
    it('should export FlagStoreOptions type', async () => {
      // Type-only test - if this compiles, the type is exported
      const { createFlagStore } = await import('../src/index');
      const options: import('../src/index').FlagStoreOptions = {
        context: { user_id: 'test-user' },
        defaultValue: false,
        realtime: true,
      };

      expect(options).toBeDefined();
    });

    it('should export FlagStoreValue type', async () => {
      // Type-only test - if this compiles, the type is exported
      const value: import('../src/index').FlagStoreValue = {
        value: true,
        loading: false,
        error: null,
        result: null,
      };

      expect(value).toBeDefined();
    });

    it('should export FlagClientConfig type', async () => {
      // Type-only test - if this compiles, the type is exported
      const config: import('../src/index').FlagClientConfig = {
        apiKey: 'test-key',
        applicationId: 'test-app',
      };

      expect(config).toBeDefined();
    });

    it('should export FlagContext type', async () => {
      // Type-only test - if this compiles, the type is exported
      const context: import('../src/index').FlagContext = {
        user_id: 'test-user',
        environment: 'test',
      };

      expect(context).toBeDefined();
    });

    it('should export FlagEvaluationResult type', async () => {
      // Type-only test - if this compiles, the type is exported
      const result: import('../src/index').FlagEvaluationResult = {
        value: true,
        flagKey: 'test-flag',
        reason: 'test-reason',
        timestamp: new Date().toISOString(),
      };

      expect(result).toBeDefined();
    });
  });

  describe('Integration with @otto-flags/svelte', () => {
    it('should allow initialization through re-exported function', async () => {
      // This test verifies that functions are properly re-exported
      // Actual initialization requires a real FlagClient instance which
      // is tested in the @otto-flags/svelte package
      const { initOttoFlags, getOttoFlags } = await import('../src/index');

      expect(initOttoFlags).toBeDefined();
      expect(getOttoFlags).toBeDefined();
      expect(typeof initOttoFlags).toBe('function');
      expect(typeof getOttoFlags).toBe('function');
    });
  });

  describe('Package structure', () => {
    it('should have separate client and server entry points', async () => {
      // Verify client entry point exists
      const clientExports = await import('../src/index');
      expect(clientExports).toBeDefined();

      // Verify server entry point exists
      const serverExports = await import('../src/server');
      expect(serverExports).toBeDefined();
    });

    it('should not expose server functions from client entry', async () => {
      const clientExports = await import('../src/index');

      // Server-only functions should not be in client exports
      expect((clientExports as any).initSvelteKitServer).toBeUndefined();
      expect((clientExports as any).getServerClient).toBeUndefined();
      expect((clientExports as any).getEventContext).toBeUndefined();
      expect((clientExports as any).evaluateForEvent).toBeUndefined();
    });
  });
});
