import { contextKey } from '../src/context-key';

describe('contextKey', () => {
  test('is independent of key order at depth', () => {
    const a = { user_id: 'u', attributes: { plan: 'pro', nested: { x: 1, y: [1, { b: 1, a: 2 }] } } };
    const b = { attributes: { nested: { y: [1, { a: 2, b: 1 }], x: 1 }, plan: 'pro' }, user_id: 'u' };
    expect(contextKey(a)).toBe(contextKey(b));
  });

  test('drops undefined fields', () => {
    expect(contextKey({ user_id: 'u', anonymous_id: undefined })).toBe(contextKey({ user_id: 'u' }));
  });

  test('different values differ', () => {
    expect(contextKey({ user_id: 'a' })).not.toBe(contextKey({ user_id: 'b' }));
    expect(contextKey({ attributes: { plan: 'pro' } })).not.toBe(contextKey({ attributes: { plan: 'free' } }));
  });

  test('returns null for a cyclic object', () => {
    const cyclic: Record<string, unknown> = {};
    cyclic.self = cyclic;
    expect(contextKey(cyclic)).toBeNull();
  });
  test('keeps an attribute named __proto__ in the key', () => {
    // JSON.parse makes "__proto__" an own property; the key must keep it, or two different
    // request bodies would share a cache entry.
    const gold = JSON.parse('{"attributes":{"__proto__":{"tier":"gold"}}}');
    const free = JSON.parse('{"attributes":{"__proto__":{"tier":"free"}}}');
    expect(contextKey(gold)).not.toBe(contextKey(free));
    expect(contextKey(gold)).toBe(JSON.stringify(gold));
  });
});
