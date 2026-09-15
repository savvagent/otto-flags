import { defineConfig } from 'astro/config';
import ottoFlags from '@otto-flags/astro';

export default defineConfig({
  integrations: [
    ottoFlags({
      apiUrl: process.env.OTTO_FLAGS_API_URL || 'http://localhost:8080',
      sdkKey: process.env.OTTO_FLAGS_SDK_KEY || 'your-sdk-key',
      environment: 'development',
    }),
  ],
});
