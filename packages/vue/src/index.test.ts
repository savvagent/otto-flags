import { describe, it, expect, vi, beforeEach } from 'vitest';
import { createApp } from 'vue';
import {
  OttoFlagsPlugin,
  getOverriddenValue,
} from './index';
import type { FlagClientConfig } from '@otto-flags/sdk';

// Mock FlagClient
vi.mock('@otto-flags/sdk', () => {
  class MockFlagClient {
    config: any;
    private mockSubscribers = new Map<string, Set<() => void>>();
    private mockOverrideSubscribers = new Set<() => void>();
    private mockOverrides: Record<string, boolean> = {};

    constructor(config: any) {
      this.config = config;
    }

    evaluate = vi.fn().mockResolvedValue({
      value: true,
      reason: 'DEFAULT',
      flagKey: 'test-flag',
    });

    isEnabled = vi.fn().mockResolvedValue(true);

    subscribe = vi.fn((flagKey: string, callback: () => void) => {
      if (!this.mockSubscribers.has(flagKey)) {
        this.mockSubscribers.set(flagKey, new Set());
      }
      this.mockSubscribers.get(flagKey)!.add(callback);
      return () => {
        this.mockSubscribers.get(flagKey)?.delete(callback);
      };
    });

    onOverrideChange = vi.fn((callback: () => void) => {
      this.mockOverrideSubscribers.add(callback);
      return () => {
        this.mockOverrideSubscribers.delete(callback);
      };
    });

    withFlag = vi.fn(async (flagKey: string, callback: () => void | Promise<void>) => {
      const result = await this.evaluate(flagKey, {});
      if (result.value) {
        await callback();
      }
    });

    setUserId = vi.fn();
    getUserId = vi.fn().mockReturnValue('test-user-123');
    getAnonymousId = vi.fn().mockReturnValue('anon-123');
    setAnonymousId = vi.fn();
    trackError = vi.fn();
    getOverrides = vi.fn(() => this.mockOverrides);

    setOverride = vi.fn((key: string, value: boolean) => {
      this.mockOverrides[key] = value;
      this.mockOverrideSubscribers.forEach(cb => cb());
    });

    clearOverride = vi.fn((key: string) => {
      delete this.mockOverrides[key];
      this.mockOverrideSubscribers.forEach(cb => cb());
    });

    clearAllOverrides = vi.fn(() => {
      this.mockOverrides = {};
      this.mockOverrideSubscribers.forEach(cb => cb());
    });
  }

  return {
    FlagClient: MockFlagClient,
    FlagClientConfig: {},
    FlagContext: {},
    FlagEvaluationResult: {},
  };
});

describe('Vue SDK', () => {
  let mockConfig: FlagClientConfig;

  beforeEach(() => {
    mockConfig = {
      apiKey: 'sdk_test_key_123',
      applicationId: 'test-app',
    };
    vi.clearAllMocks();
  });

  describe('OttoFlagsPlugin', () => {
    it('should install plugin with config only', () => {
      const app = createApp({});
      OttoFlagsPlugin.install(app, mockConfig);

      expect(app.config.globalProperties.$ottoFlags).toBeDefined();
      expect(app.config.globalProperties.$ottoFlags.config).toEqual(mockConfig);
    });

    it('should install plugin with full options format', () => {
      const app = createApp({});
      const defaultContext = {
        environment: 'development',
        userId: 'user-123',
      };

      OttoFlagsPlugin.install(app, {
        config: mockConfig,
        defaultContext,
      });

      expect(app.config.globalProperties.$ottoFlags).toBeDefined();
      expect(app.config.globalProperties.$ottoFlags.config).toEqual(mockConfig);
    });

    it('should support legacy format (config only)', () => {
      const app = createApp({});
      OttoFlagsPlugin.install(app, mockConfig);

      expect(app.config.globalProperties.$ottoFlags).toBeDefined();
    });

    it('should support new format with default context', () => {
      const app = createApp({});
      OttoFlagsPlugin.install(app, {
        config: mockConfig,
        defaultContext: {
          environment: 'production',
          userId: 'user-456',
        },
      });

      expect(app.config.globalProperties.$ottoFlags).toBeDefined();
    });
  });

  describe('getOverriddenValue', () => {
    it('should return override value when present', () => {
      const overrides = {
        'flag-1': true,
        'flag-2': false,
      };

      expect(getOverriddenValue('flag-1', false, overrides)).toBe(true);
      expect(getOverriddenValue('flag-2', true, overrides)).toBe(false);
    });

    it('should return server value when no override', () => {
      const overrides = {
        'flag-1': true,
      };

      expect(getOverriddenValue('flag-2', true, overrides)).toBe(true);
      expect(getOverriddenValue('flag-3', false, overrides)).toBe(false);
    });

    it('should handle empty overrides object', () => {
      const overrides = {};

      expect(getOverriddenValue('flag-1', true, overrides)).toBe(true);
      expect(getOverriddenValue('flag-2', false, overrides)).toBe(false);
    });

    it('should distinguish undefined from false', () => {
      const overrides = {
        'flag-1': false,
      };

      expect(getOverriddenValue('flag-1', true, overrides)).toBe(false);
      expect(getOverriddenValue('flag-2', true, overrides)).toBe(true);
    });

    it('should handle mixed override values', () => {
      const overrides = {
        'enabled-feature': true,
        'disabled-feature': false,
      };

      expect(getOverriddenValue('enabled-feature', false, overrides)).toBe(true);
      expect(getOverriddenValue('disabled-feature', true, overrides)).toBe(false);
      expect(getOverriddenValue('default-feature', true, overrides)).toBe(true);
    });
  });

  describe('Type Safety', () => {
    it('should export all required types', () => {
      // This test verifies that the exports are properly typed
      // TypeScript will fail to compile if types are missing
      expect(OttoFlagsPlugin).toBeDefined();
      expect(getOverriddenValue).toBeDefined();
    });
  });
});

describe('Vue SDK Integration', () => {
  it('should be importable without errors', async () => {
    const module = await import('./index');

    expect(module.OttoFlagsPlugin).toBeDefined();
    expect(module.provideOttoFlags).toBeDefined();
    expect(module.useOttoFlags).toBeDefined();
    expect(module.useFlag).toBeDefined();
    expect(module.useFlags).toBeDefined();
    expect(module.useWithFlag).toBeDefined();
    expect(module.useUser).toBeDefined();
    expect(module.useTrackError).toBeDefined();
    expect(module.useLocalOverrides).toBeDefined();
    expect(module.getOverriddenValue).toBeDefined();
    expect(module.FlagClient).toBeDefined();
  });

  it('should export expected number of composables', async () => {
    const module = await import('./index');
    const exports = Object.keys(module);

    // Verify we have the main exports
    expect(exports).toContain('OttoFlagsPlugin');
    expect(exports).toContain('provideOttoFlags');
    expect(exports).toContain('useOttoFlags');
    expect(exports).toContain('useFlag');
    expect(exports).toContain('useFlags');
    expect(exports).toContain('useWithFlag');
    expect(exports).toContain('useUser');
    expect(exports).toContain('useTrackError');
    expect(exports).toContain('useLocalOverrides');
    expect(exports).toContain('getOverriddenValue');
    expect(exports).toContain('FlagClient');
  });
});
