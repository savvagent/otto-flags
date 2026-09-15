import { NgModule, ModuleWithProviders } from '@angular/core';
import { OttoFlagsService, OttoFlagsConfig, OTTO_FLAGS_CONFIG } from './service';

/**
 * Angular module for Otto Flags feature flags.
 *
 * @example
 * ```typescript
 * // app.module.ts
 * import { OttoFlagsModule } from '@otto-flags/angular';
 *
 * @NgModule({
 *   imports: [
 *     OttoFlagsModule.forRoot({
 *       config: {
 *         apiKey: 'sdk_your_api_key',
 *         baseUrl: 'https://flags-api.otto-flags.dev'
 *       },
 *       defaultContext: {
 *         applicationId: 'my-app',
 *         environment: 'production',
 *         userId: 'user-123'
 *       }
 *     })
 *   ]
 * })
 * export class AppModule {}
 * ```
 *
 * @example
 * ```typescript
 * // For standalone components (Angular 14+)
 * import { OttoFlagsModule } from '@otto-flags/angular';
 *
 * bootstrapApplication(AppComponent, {
 *   providers: [
 *     importProvidersFrom(
 *       OttoFlagsModule.forRoot({
 *         config: { apiKey: 'sdk_...' }
 *       })
 *     )
 *   ]
 * });
 * ```
 */
@NgModule({
  providers: [OttoFlagsService]
})
export class OttoFlagsModule {
  /**
   * Configure the Otto Flags module with API key and default context.
   *
   * @param ottoFlagsConfig - Configuration including API key and optional default context
   * @returns Module with providers
   */
  static forRoot(ottoFlagsConfig: OttoFlagsConfig): ModuleWithProviders<OttoFlagsModule> {
    return {
      ngModule: OttoFlagsModule,
      providers: [
        {
          provide: OTTO_FLAGS_CONFIG,
          useValue: ottoFlagsConfig
        },
        OttoFlagsService
      ]
    };
  }
}
