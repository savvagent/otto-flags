/// <reference types="svelte" />
/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_OTTO_FLAGS_API_URL: string;
  readonly VITE_OTTO_FLAGS_SDK_KEY: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
