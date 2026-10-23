import { CacheEntry } from './types';

/** Most evaluation contexts cached for one flag. */
export const MAX_CONTEXTS_PER_FLAG = 10_000;

/**
 * Simple in-memory cache for flag values with TTL, keyed by flag and evaluation context.
 *
 * The server's answer depends on the context sent, so one context's value is never served for
 * another. Contexts are bounded per flag; hitting the bound costs misses, never a wrong answer.
 */
export class FlagCache {
  private cache: Map<string, Map<string, CacheEntry>> = new Map();
  private ttl: number;
  private maxContextsPerFlag = MAX_CONTEXTS_PER_FLAG;

  constructor(ttl: number = 60000) {
    this.ttl = ttl;
  }

  /** Bumped by every invalidation so a write from a request that started before it is dropped. */
  private epoch = 0;
  private generations = new Map<string, number>();

  /**
   * The flag's invalidation generation. Read it before sending an evaluate request and pass it to
   * `set`, which drops the write if the flag was invalidated since (the result is stale).
   */
  generation(key: string): string {
    return `${this.epoch}.${this.generations.get(key) ?? 0}`;
  }

  private bump(key?: string): void {
    if (key) {
      this.generations.set(key, (this.generations.get(key) ?? 0) + 1);
    } else {
      this.epoch++;
      this.generations.clear();
    }
  }

  /**
   * Get a cached flag value for a context
   */
  get(key: string, contextKey: string = ''): boolean | null {
    const contexts = this.cache.get(key);
    const entry = contexts?.get(contextKey);
    if (!contexts || !entry) {
      return null;
    }

    // Check if expired
    if (Date.now() > entry.expiresAt) {
      contexts.delete(contextKey);
      if (contexts.size === 0) {
        this.cache.delete(key);
      }
      return null;
    }

    return entry.value;
  }

  /**
   * Set a flag value in cache for a context
   */
  set(key: string, value: boolean, flagId?: string, contextKey: string = '', generation?: string): void {
    if (generation !== undefined && generation !== this.generation(key)) return;
    let contexts = this.cache.get(key);
    if (!contexts) {
      contexts = new Map();
      this.cache.set(key, contexts);
    }

    if (contexts.size >= this.maxContextsPerFlag && !contexts.has(contextKey)) {
      // Full: drop expired entries; if more than half the bound is still live, clear the flag.
      // Either way at least half the bound is free, so the sweep runs rarely.
      const now = Date.now();
      for (const [ctx, entry] of contexts) {
        if (now > entry.expiresAt) {
          contexts.delete(ctx);
        }
      }
      if (contexts.size > this.maxContextsPerFlag / 2) {
        contexts.clear();
      }
    }

    contexts.set(contextKey, {
      value,
      expiresAt: Date.now() + this.ttl,
      flagId,
    });
  }

  /**
   * Invalidate a specific flag, for every context
   */
  invalidate(key: string): void {
    this.bump(key);
    this.cache.delete(key);
  }

  /**
   * Clear all cached flags
   */
  clear(): void {
    this.bump();
    this.cache.clear();
  }

  /**
   * Get the keys of flags with at least one live entry
   */
  keys(): string[] {
    const now = Date.now();
    const keys: string[] = [];
    for (const [key, contexts] of this.cache) {
      for (const entry of contexts.values()) {
        if (now <= entry.expiresAt) {
          keys.push(key);
          break;
        }
      }
    }
    return keys;
  }
}
