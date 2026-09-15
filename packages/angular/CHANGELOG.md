# @otto-flags/angular

## 1.1.0

### Minor Changes

- Add environment support with setEnvironment/getEnvironment methods across all SDKs for dynamic environment switching at runtime.

### Patch Changes

- Updated dependencies
  - @otto-flags/sdk@1.1.0

## 1.0.1

### Patch Changes

- Updates for new API parameters
- Updated dependencies
  - @otto-flags/sdk@1.0.1

## 1.0.0

### Features

- Initial release of Angular SDK for Otto Flags feature flags
- `OttoFlagsModule` - Angular module for easy SDK configuration with `forRoot()`
- `OttoFlagsService` - Injectable service with full feature flag functionality
- Reactive API with RxJS Observables (`flag$`, `flagValue$`, `getAllFlags$`)
- Promise-based API for non-reactive use cases (`evaluate`, `isEnabled`, `withFlag`)
- Real-time flag updates via SSE subscription
- Local override support for development and testing
- User identification and anonymous ID management
- Error tracking with flag context for AI-powered analysis
- Full TypeScript support with comprehensive type definitions
- Support for Angular 14+ (including standalone components)
- Default context configuration for application-wide settings
