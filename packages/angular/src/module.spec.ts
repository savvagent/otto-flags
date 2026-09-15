import { TestBed } from '@angular/core/testing';
import { OttoFlagsModule } from './module';
import { OttoFlagsService, OTTO_FLAGS_CONFIG, OttoFlagsConfig } from './service';

describe('OttoFlagsModule', () => {
  afterEach(() => {
    TestBed.resetTestingModule();
  });

  describe('Module Configuration', () => {
    it('should create module', () => {
      const module = new OttoFlagsModule();
      expect(module).toBeTruthy();
    });

    it('should provide OttoFlagsService by default', () => {
      TestBed.configureTestingModule({
        imports: [OttoFlagsModule],
      });

      const service = TestBed.inject(OttoFlagsService);
      expect(service).toBeTruthy();
    });
  });

  describe('forRoot Configuration', () => {
    const testConfig: OttoFlagsConfig = {
      config: {
        apiKey: 'sdk_test_api_key',
        baseUrl: 'https://api.test.com',
      },
      defaultContext: {
        applicationId: 'test-app',
        environment: 'test',
      },
    };

    it('should return ModuleWithProviders', () => {
      const moduleWithProviders = OttoFlagsModule.forRoot(testConfig);

      expect(moduleWithProviders).toEqual({
        ngModule: OttoFlagsModule,
        providers: [
          {
            provide: OTTO_FLAGS_CONFIG,
            useValue: testConfig,
          },
          OttoFlagsService,
        ],
      });
    });

    it('should provide OTTO_FLAGS_CONFIG token', () => {
      TestBed.configureTestingModule({
        imports: [OttoFlagsModule.forRoot(testConfig)],
      });

      const config = TestBed.inject(OTTO_FLAGS_CONFIG);
      expect(config).toEqual(testConfig);
    });

    it('should initialize OttoFlagsService with config', () => {
      TestBed.configureTestingModule({
        imports: [OttoFlagsModule.forRoot(testConfig)],
      });

      const service = TestBed.inject(OttoFlagsService);
      expect(service).toBeTruthy();
      expect(service.isReady).toBe(true);
    });

    it('should work with minimal config', () => {
      const minimalConfig: OttoFlagsConfig = {
        config: {
          apiKey: 'sdk_test_key',
        },
      };

      TestBed.configureTestingModule({
        imports: [OttoFlagsModule.forRoot(minimalConfig)],
      });

      const service = TestBed.inject(OttoFlagsService);
      expect(service).toBeTruthy();
      expect(service.isReady).toBe(true);
    });

    it('should work with full config including all context fields', () => {
      const fullConfig: OttoFlagsConfig = {
        config: {
          apiKey: 'sdk_test_api_key',
          baseUrl: 'https://api.test.com',
          enableRealtime: true,
          cacheTtl: 60000,
          onError: (error) => console.error(error),
        },
        defaultContext: {
          applicationId: 'test-app',
          environment: 'production',
          organizationId: 'org-123',
          userId: 'user-456',
          anonymousId: 'anon-789',
          sessionId: 'session-abc',
          language: 'en',
          attributes: {
            plan: 'pro',
            region: 'us-west',
          },
        },
      };

      TestBed.configureTestingModule({
        imports: [OttoFlagsModule.forRoot(fullConfig)],
      });

      const config = TestBed.inject(OTTO_FLAGS_CONFIG);
      expect(config).toEqual(fullConfig);

      const service = TestBed.inject(OttoFlagsService);
      expect(service).toBeTruthy();
      expect(service.isReady).toBe(true);
    });
  });

  describe('Dependency Injection', () => {
    it('should provide same service instance within module scope', () => {
      TestBed.configureTestingModule({
        imports: [
          OttoFlagsModule.forRoot({
            config: { apiKey: 'test' },
          }),
        ],
      });

      const service1 = TestBed.inject(OttoFlagsService);
      const service2 = TestBed.inject(OttoFlagsService);

      expect(service1).toBe(service2);
    });

    it('should work without forRoot (service providedIn: root)', () => {
      TestBed.configureTestingModule({
        imports: [OttoFlagsModule],
      });

      const service = TestBed.inject(OttoFlagsService);
      expect(service).toBeTruthy();
      // Service won't be initialized without config
      expect(service.isReady).toBe(false);
    });
  });

  describe('Multiple Imports', () => {
    it('should handle multiple module imports', () => {
      const config1: OttoFlagsConfig = {
        config: { apiKey: 'key1' },
      };
      const config2: OttoFlagsConfig = {
        config: { apiKey: 'key2' },
      };

      // The last imported config should win
      TestBed.configureTestingModule({
        imports: [
          OttoFlagsModule.forRoot(config1),
          OttoFlagsModule.forRoot(config2),
        ],
      });

      const config = TestBed.inject(OTTO_FLAGS_CONFIG);
      // Due to Angular's DI, the first provider typically wins,
      // but this tests that the setup doesn't break
      expect(config).toBeDefined();
    });
  });

  describe('Integration Tests', () => {
    it('should allow service to evaluate flags after module setup', async () => {
      TestBed.configureTestingModule({
        imports: [
          OttoFlagsModule.forRoot({
            config: { apiKey: 'sdk_test' },
            defaultContext: {
              applicationId: 'test-app',
            },
          }),
        ],
      });

      const service = TestBed.inject(OttoFlagsService);
      expect(service).toBeTruthy();
      expect(service.isReady).toBe(true);
      expect(service.flagClient).not.toBeNull();
    });

    it('should support standalone component pattern (Angular 14+)', () => {
      // Simulate standalone component setup
      const providers = OttoFlagsModule.forRoot({
        config: { apiKey: 'sdk_test' },
      }).providers || [];

      TestBed.configureTestingModule({
        providers,
      });

      const service = TestBed.inject(OttoFlagsService);
      expect(service).toBeTruthy();
      expect(service.isReady).toBe(true);
    });
  });

  describe('Error Handling', () => {
    it('should handle invalid config gracefully', () => {
      const invalidConfig = {
        config: {} as any, // Missing apiKey
      };

      expect(() => {
        TestBed.configureTestingModule({
          imports: [OttoFlagsModule.forRoot(invalidConfig)],
        });
      }).not.toThrow();
    });

    it('should handle null/undefined values in config', () => {
      const configWithNulls: OttoFlagsConfig = {
        config: {
          apiKey: 'test',
          baseUrl: undefined,
        },
        defaultContext: {
          applicationId: undefined,
          environment: undefined,
        },
      };

      TestBed.configureTestingModule({
        imports: [OttoFlagsModule.forRoot(configWithNulls)],
      });

      const service = TestBed.inject(OttoFlagsService);
      expect(service).toBeTruthy();
    });
  });

  describe('Type Safety', () => {
    it('should enforce correct config structure', () => {
      const validConfig: OttoFlagsConfig = {
        config: {
          apiKey: 'test_api_key',
        },
        defaultContext: {
          userId: 'user-123',
        },
      };

      const moduleWithProviders = OttoFlagsModule.forRoot(validConfig);
      expect(moduleWithProviders.ngModule).toBe(OttoFlagsModule);
    });

    it('should accept all valid defaultContext properties', () => {
      const config: OttoFlagsConfig = {
        config: { apiKey: 'test' },
        defaultContext: {
          applicationId: 'app',
          environment: 'prod',
          organizationId: 'org',
          userId: 'user',
          anonymousId: 'anon',
          sessionId: 'session',
          language: 'en',
          attributes: { key: 'value' },
        },
      };

      expect(() => {
        TestBed.configureTestingModule({
          imports: [OttoFlagsModule.forRoot(config)],
        });
      }).not.toThrow();
    });
  });
});
