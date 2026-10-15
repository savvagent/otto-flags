import { contextKey } from './context-key';

describe('contextKey', () => {
  it('is independent of key order at every depth', () => {
    const a = { user_id: 'u', attributes: { plan: 'pro', geo: { c: 1, d: 2 } } };
    const b = { attributes: { geo: { d: 2, c: 1 }, plan: 'pro' }, user_id: 'u' };
    expect(contextKey(a)).toBe(contextKey(b));
  });

  it('drops undefined fields, equal to omitted', () => {
    expect(contextKey({ user_id: 'u', session_id: undefined })).toBe(contextKey({ user_id: 'u' }));
  });

  it('differs when a value differs', () => {
    expect(contextKey({ attributes: { plan: 'pro' } })).not.toBe(contextKey({ attributes: { plan: 'free' } }));
  });

  it('keeps array order significant', () => {
    expect(contextKey({ a: [1, 2] })).not.toBe(contextKey({ a: [2, 1] }));
  });

  it('returns null for a cyclic object', () => {
    const cyclic: any = { user_id: 'u' };
    cyclic.self = cyclic;
    expect(contextKey(cyclic)).toBeNull();
  });
});
