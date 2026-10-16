import { CacheEntry } from './types';

/**
 * Most contexts cached for one flag. When a new context finds the flag full, its expired
 * entries are swept, and if more than half the bound is still live the flag is cleared, so
 * at least half the bound is free afterwards. The cost of the bound is cache misses, never
 * a wrong answer.
 */
const MAX_CONTEXTS_PER_FLAG = 10_000;

/**
 * In-memory cache for flag values with TTL, keyed by flag and then by evaluation context
 * (see `contextKey`), so a result is reused only for the context that produced it.
 */
export class FlagCache {
  private cache: Map<string, Map<string, CacheEntry>>;
  private ttl: number;
  private maxContextsPerFlag = MAX_CONTEXTS_PER_FLAG;

  constructor(ttl: number = 60000) {
    this.cache = new Map();
    this.ttl = ttl;
  }

  /**
   * Get a cached flag value
   */
  get(key: string, contextKey: string = ''): boolean | null {
    const entry = this.getEntry(key, contextKey);
    return entry ? entry.value : null;
  }

  /**
   * Get a complete cached entry (includes configuration and variation)
   */
  getEntry(key: string, contextKey: string = ''): CacheEntry | null {
    const contexts = this.cache.get(key);
    const entry = contexts?.get(contextKey);
    if (!contexts || !entry) return null;

    // Check if expired
    if (Date.now() > entry.expiresAt) {
      contexts.delete(contextKey);
      if (contexts.size === 0) this.cache.delete(key);
      return null;
    }

    return entry;
  }

  /**
   * Set a flag value in cache
   */
  set(
    key: string,
    value: boolean,
    flagId?: string,
    configuration?: any,
    variation?: string,
    contextKey: string = ''
  ): void {
    let contexts = this.cache.get(key);
    if (!contexts) {
      contexts = new Map();
      this.cache.set(key, contexts);
    }
    if (contexts.size >= this.maxContextsPerFlag && !contexts.has(contextKey)) {
      const now = Date.now();
      for (const [k, e] of contexts) {
        if (now > e.expiresAt) contexts.delete(k);
      }
      if (contexts.size > this.maxContextsPerFlag / 2) contexts.clear();
    }
    contexts.set(contextKey, {
      value,
      configuration,
      variation,
      expiresAt: Date.now() + this.ttl,
      flagId,
    });
  }

  /**
   * Invalidate a specific flag (every context) or all flags
   */
  invalidate(key?: string): void {
    if (key) {
      this.cache.delete(key);
    } else {
      this.cache.clear();
    }
  }

  /**
   * Clear all cached values
   */
  clear(): void {
    this.cache.clear();
  }
}
