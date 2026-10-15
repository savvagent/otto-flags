import { FlagCache } from '../src/cache';

describe('FlagCache', () => {
  let cache: FlagCache;

  beforeEach(() => {
    cache = new FlagCache(1000); // 1 second TTL
  });

  test('should cache and retrieve values', () => {
    cache.set('test-flag', true);
    expect(cache.get('test-flag')).toBe(true);
  });

  test('should return null for non-existent keys', () => {
    expect(cache.get('non-existent')).toBeNull();
  });

  test('should expire cached values after TTL', (done) => {
    cache.set('test-flag', true);
    expect(cache.get('test-flag')).toBe(true);

    setTimeout(() => {
      expect(cache.get('test-flag')).toBeNull();
      done();
    }, 1100);
  });

  test('should invalidate specific keys', () => {
    cache.set('flag1', true);
    cache.set('flag2', false);

    cache.invalidate('flag1');

    expect(cache.get('flag1')).toBeNull();
    expect(cache.get('flag2')).toBe(false);
  });

  test('should clear all cached values', () => {
    cache.set('flag1', true);
    cache.set('flag2', false);

    cache.clear();

    expect(cache.get('flag1')).toBeNull();
    expect(cache.get('flag2')).toBeNull();
  });

  test('should return all cached keys', () => {
    cache.set('flag1', true);
    cache.set('flag2', false);

    const keys = cache.keys();
    expect(keys).toContain('flag1');
    expect(keys).toContain('flag2');
    expect(keys.length).toBe(2);
  });

  test('caches per context and misses on another context', () => {
    cache.set('flag', true, undefined, 'ctx-a');
    expect(cache.get('flag', 'ctx-a')).toBe(true);
    expect(cache.get('flag', 'ctx-b')).toBeNull();
    expect(cache.get('flag')).toBeNull();
  });

  test('invalidate drops every context of a flag', () => {
    cache.set('flag', true, undefined, 'ctx-a');
    cache.set('flag', false, undefined, 'ctx-b');
    cache.invalidate('flag');
    expect(cache.get('flag', 'ctx-a')).toBeNull();
    expect(cache.get('flag', 'ctx-b')).toBeNull();
  });

  test('keys lists flags with a live entry only', () => {
    cache.set('flag', true, undefined, 'ctx-a');
    expect(cache.keys()).toEqual(['flag']);
    jest.useFakeTimers();
    jest.setSystemTime(Date.now() + 5000);
    expect(cache.keys()).toEqual([]);
    jest.useRealTimers();
  });

  test('bounds contexts per flag and keeps the newest', () => {
    (cache as any).maxContextsPerFlag = 4;
    for (let i = 0; i < 10; i++) {
      cache.set('flag', true, undefined, `ctx-${i}`);
      expect((cache as any).cache.get('flag').size).toBeLessThanOrEqual(4);
    }
    expect(cache.get('flag', 'ctx-9')).toBe(true);
  });

  test('sweeps expired entries before clearing a full flag', () => {
    (cache as any).maxContextsPerFlag = 4;
    jest.useFakeTimers();
    const start = Date.now();
    jest.setSystemTime(start);
    cache.set('flag', true, undefined, 'old-1');
    cache.set('flag', true, undefined, 'old-2');
    jest.setSystemTime(start + 900);
    cache.set('flag', true, undefined, 'live-1');
    cache.set('flag', true, undefined, 'live-2');
    jest.setSystemTime(start + 1100);
    cache.set('flag', true, undefined, 'new');
    expect(cache.get('flag', 'live-1')).toBe(true);
    expect(cache.get('flag', 'new')).toBe(true);
    jest.useRealTimers();
  });
});
