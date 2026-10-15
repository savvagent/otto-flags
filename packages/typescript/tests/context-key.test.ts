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
});
