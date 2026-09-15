# @otto-flags/angular

Angular SDK for Otto Flags - AI-powered feature flags that prevent production incidents.

## Installation

```bash
npm install @otto-flags/angular
# or
pnpm add @otto-flags/angular
# or
yarn add @otto-flags/angular
```

## Quick Start

### 1. Import OttoFlagsModule in your app

```typescript
// app.module.ts
import { OttoFlagsModule } from '@otto-flags/angular';

@NgModule({
  imports: [
    OttoFlagsModule.forRoot({
      config: {
        apiKey: 'sdk_your_api_key_here',
        applicationId: 'your-app-id', // Optional: for application-scoped flags
        enableRealtime: true, // Enable real-time flag updates
      },
      defaultContext: {
        environment: 'production',
        userId: 'user-123', // Optional: set default user
      }
    })
  ]
})
export class AppModule {}
```

### 2. Use OttoFlagsService in your components

```typescript
import { Component } from '@angular/core';
import { OttoFlagsService } from '@otto-flags/angular';

@Component({
  selector: 'app-my-feature',
  template: `
    <ng-container *ngIf="newFeature$ | async as flag">
      <app-spinner *ngIf="flag.loading"></app-spinner>
      <app-new-checkout *ngIf="flag.value"></app-new-checkout>
      <app-old-checkout *ngIf="!flag.value && !flag.loading"></app-old-checkout>
    </ng-container>
  `
})
export class MyFeatureComponent {
  newFeature$ = this.ottoFlags.flag$('new-checkout-flow', {
    defaultValue: false,
    realtime: true,
  });

  constructor(private ottoFlags: OttoFlagsService) {}
}
```

## Standalone Components (Angular 14+)

For standalone components, you can use `importProvidersFrom`:

```typescript
// main.ts
import { bootstrapApplication } from '@angular/platform-browser';
import { importProvidersFrom } from '@angular/core';
import { OttoFlagsModule } from '@otto-flags/angular';
import { AppComponent } from './app/app.component';

bootstrapApplication(AppComponent, {
  providers: [
    importProvidersFrom(
      OttoFlagsModule.forRoot({
        config: { apiKey: 'sdk_your_api_key' }
      })
    )
  ]
});
```

## API Reference

### `OttoFlagsModule`

Angular module that configures the Otto Flags SDK.

#### `OttoFlagsModule.forRoot(config)`

Configure the module with your API key and default context.

```typescript
interface OttoFlagsConfig {
  config: FlagClientConfig;
  defaultContext?: DefaultFlagContext;
}

interface FlagClientConfig {
  /** SDK API key (starts with sdk_) */
  apiKey: string;
  /** Application ID for application-scoped flags */
  applicationId?: string;
  /** Base URL for the Otto Flags API */
  baseUrl?: string;
  /** Enable real-time flag updates via SSE (default: true) */
  enableRealtime?: boolean;
  /** Cache TTL in milliseconds (default: 60000) */
  cacheTtl?: number;
  /** Enable telemetry tracking (default: true) */
  enableTelemetry?: boolean;
  /** Default flag values when evaluation fails */
  defaults?: Record<string, boolean>;
  /** Custom error handler */
  onError?: (error: Error) => void;
}

interface DefaultFlagContext {
  applicationId?: string;
  environment?: string;
  organizationId?: string;
  userId?: string;
  anonymousId?: string;
  sessionId?: string;
  language?: string;
  attributes?: Record<string, any>;
}
```

### `OttoFlagsService`

Injectable service that provides all feature flag functionality.

#### Properties

- `ready$: Observable<boolean>` - Observable that emits true when the client is ready
- `isReady: boolean` - Check if the client is ready synchronously
- `flagClient: FlagClient | null` - Access the underlying FlagClient for advanced use cases

#### `flag$(flagKey, options)`

Get a reactive Observable for a feature flag with automatic updates.

```typescript
interface FlagOptions {
  /** Context for flag evaluation (user_id, attributes, etc.) */
  context?: FlagContext;
  /** Default value to use while loading or on error */
  defaultValue?: boolean;
  /** Enable real-time updates for this flag (default: true) */
  realtime?: boolean;
}

interface FlagObservableResult {
  /** Current flag value */
  value: boolean;
  /** Whether the flag is currently being evaluated */
  loading: boolean;
  /** Error if evaluation failed */
  error: Error | null;
  /** Detailed evaluation result */
  result: FlagEvaluationResult | null;
}
```

**Example:**

```typescript
@Component({
  template: `
    <ng-container *ngIf="betaFeature$ | async as flag">
      <div *ngIf="flag.loading">Loading...</div>
      <div *ngIf="flag.error">Error: {{ flag.error.message }}</div>
      <app-beta *ngIf="flag.value"></app-beta>
      <app-standard *ngIf="!flag.value && !flag.loading"></app-standard>
    </ng-container>
  `
})
export class MyComponent {
  betaFeature$ = this.ottoFlags.flag$('beta-feature', {
    context: {
      user_id: this.userId,
      attributes: { plan: 'pro' }
    },
    defaultValue: false,
    realtime: true
  });

  constructor(private ottoFlags: OttoFlagsService) {}
}
```

#### `flagValue$(flagKey, options)`

Get just the boolean value as an Observable. Useful when you don't need loading/error states.

```typescript
@Component({
  template: `
    <button *ngIf="isFeatureEnabled$ | async">New Button</button>
  `
})
export class SimpleComponent {
  isFeatureEnabled$ = this.ottoFlags.flagValue$('my-feature');

  constructor(private ottoFlags: OttoFlagsService) {}
}
```

#### `evaluate(flagKey, context)`

Evaluate a feature flag once (non-reactive).

```typescript
async checkFeature() {
  const result = await this.ottoFlags.evaluate('new-feature');
  console.log(result.value, result.reason);
}
```

#### `isEnabled(flagKey, context)`

Simple boolean check if a flag is enabled.

```typescript
async doSomething() {
  if (await this.ottoFlags.isEnabled('feature-flag')) {
    // Feature is enabled
  }
}
```

#### `withFlag(flagKey, callback, context)`

Execute code conditionally based on flag value.

```typescript
async trackPageView() {
  await this.ottoFlags.withFlag('analytics-enabled', async () => {
    await this.analytics.track('page_view');
  });
}
```

#### `trackError(flagKey, error, context)`

Track errors with flag context for AI-powered analysis.

```typescript
handleError(error: Error) {
  this.ottoFlags.trackError('new-payment-flow', error);
}
```

#### User Management

```typescript
// Set user ID for logged-in users
setUserId(userId: string | null): void;
getUserId(): string | null;

// Anonymous ID management
getAnonymousId(): string | null;
setAnonymousId(id: string): void;
```

#### Local Overrides

For development and testing:

```typescript
// Set override (takes precedence over server values)
setOverride(flagKey: string, value: boolean): void;

// Clear overrides
clearOverride(flagKey: string): void;
clearAllOverrides(): void;

// Check overrides
hasOverride(flagKey: string): boolean;
getOverride(flagKey: string): boolean | undefined;
getOverrides(): Record<string, boolean>;

// Set multiple overrides
setOverrides(overrides: Record<string, boolean>): void;
```

#### Flag Discovery

```typescript
// Get all flags (returns Observable)
getAllFlags$(environment?: string): Observable<FlagDefinition[]>;

// Get all flags (Promise-based)
getAllFlags(environment?: string): Promise<FlagDefinition[]>;

// Get enterprise-scoped flags only
getEnterpriseFlags(environment?: string): Promise<FlagDefinition[]>;
```

#### Cache & Connection

```typescript
clearCache(): void;
isRealtimeConnected(): boolean;
close(): void;
```

## Advanced Examples

### User Targeting

```typescript
@Component({...})
export class UserFeatureComponent implements OnInit {
  premiumFeature$!: Observable<FlagObservableResult>;

  constructor(
    private ottoFlags: OttoFlagsService,
    private auth: AuthService
  ) {}

  ngOnInit() {
    this.premiumFeature$ = this.ottoFlags.flag$('premium-features', {
      context: {
        user_id: this.auth.userId,
        attributes: {
          plan: this.auth.userPlan,
          signupDate: this.auth.signupDate
        }
      }
    });
  }
}
```

### Dynamic Initialization

If you need to initialize the service after getting user data:

```typescript
@Component({...})
export class AppComponent implements OnInit {
  constructor(
    private ottoFlags: OttoFlagsService,
    private auth: AuthService
  ) {}

  ngOnInit() {
    // Wait for auth, then initialize
    this.auth.user$.pipe(take(1)).subscribe(user => {
      this.ottoFlags.initialize({
        config: {
          apiKey: environment.ottoFlagsApiKey
        },
        defaultContext: {
          userId: user?.id,
          environment: environment.name
        }
      });
    });
  }
}
```

### Error Tracking

```typescript
@Component({...})
export class PaymentComponent {
  constructor(private ottoFlags: OttoFlagsService) {}

  async processPayment() {
    try {
      const result = await this.paymentService.process();
      return result;
    } catch (error) {
      // Error is correlated with flag changes
      this.ottoFlags.trackError('new-payment-flow', error as Error);
      throw error;
    }
  }
}
```

### A/B Testing

```typescript
@Component({
  template: `
    <app-checkout-a *ngIf="!(variantB$ | async)"></app-checkout-a>
    <app-checkout-b *ngIf="variantB$ | async"></app-checkout-b>
  `
})
export class ABTestComponent {
  variantB$ = this.ottoFlags.flagValue$('checkout-variant-b', {
    context: {
      user_id: this.userId // Consistent assignment per user
    }
  });

  constructor(private ottoFlags: OttoFlagsService) {}
}
```

### Development Override Panel

```typescript
@Component({
  selector: 'app-flag-overrides',
  template: `
    <div *ngFor="let flag of flags$ | async">
      <label>
        <input
          type="checkbox"
          [checked]="ottoFlags.getOverride(flag.key) ?? flag.enabled"
          (change)="toggleOverride(flag.key, $event)"
        />
        {{ flag.key }}
      </label>
      <button (click)="clearOverride(flag.key)">Reset</button>
    </div>
  `
})
export class FlagOverridesComponent implements OnInit {
  flags$ = this.ottoFlags.getAllFlags$('development');

  constructor(public ottoFlags: OttoFlagsService) {}

  toggleOverride(flagKey: string, event: Event) {
    const checked = (event.target as HTMLInputElement).checked;
    this.ottoFlags.setOverride(flagKey, checked);
  }

  clearOverride(flagKey: string) {
    this.ottoFlags.clearOverride(flagKey);
  }
}
```

## TypeScript Support

This package is written in TypeScript and provides full type definitions.

```typescript
import type {
  FlagClientConfig,
  FlagContext,
  FlagEvaluationResult,
  FlagDefinition,
  OttoFlagsConfig,
  DefaultFlagContext,
  FlagObservableResult,
  FlagOptions,
} from '@otto-flags/angular';
```

## Best Practices

1. **Import OttoFlagsModule.forRoot() in your root module** to ensure a single instance of the service.

2. **Use the `defaultValue` option** to provide a safe fallback while flags are loading.

3. **Enable real-time updates** for flags that change frequently or require immediate propagation.

4. **Track errors** in new features to leverage Otto Flags's AI-powered error correlation.

5. **Use user context** for targeted rollouts based on user attributes, location, or behavior.

6. **Handle loading states** gracefully using the async pipe and conditional rendering.

7. **Use `flagValue$`** when you only need the boolean value without loading/error states.

8. **Clean up subscriptions** - the service handles cleanup automatically on destroy, but use `takeUntil` or similar patterns in components for long-lived subscriptions.

## License

MIT
