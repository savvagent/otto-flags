import { FlagClient } from '../src/client';

describe('FlagClient', () => {
  test('should throw error for invalid API key', () => {
    expect(() => {
      new FlagClient({ apiKey: 'invalid-key' });
    }).toThrow('Invalid API key');
  });

  test('should accept valid API key', () => {
    expect(() => {
      new FlagClient({ apiKey: 'sdk_test_key' });
    }).not.toThrow();
  });

  test('should apply default configuration', () => {
    const client = new FlagClient({
      apiKey: 'sdk_test_key',
    });

    expect(client).toBeDefined();
    // Client should be initialized with defaults
  });

  test('should accept custom configuration', () => {
    const client = new FlagClient({
      apiKey: 'sdk_test_key',
      baseUrl: 'https://custom.api.com',
      enableRealtime: false,
      enableTelemetry: false,
      cacheTtl: 30000,
      defaults: {
        'test-flag': true,
      },
    });

    expect(client).toBeDefined();
  });

  // Note: More comprehensive integration tests would require mocking fetch
  // and EventSource, which is beyond the scope of this basic test suite

  describe('flag key validation (#107)', () => {
    const baseUrl = 'https://flags.example.test';
    const invalidKeys = ['a/b', 'a?b', 'a#b', '.', '..', '', 'bad key', 'a'.repeat(129)];
    let fetchMock: jest.Mock;
    let originalFetch: typeof global.fetch;

    beforeEach(() => {
      originalFetch = global.fetch;
      fetchMock = jest.fn();
      global.fetch = fetchMock as any;
    });

    afterEach(() => {
      global.fetch = originalFetch;
    });

    const makeClient = (onError: jest.Mock) =>
      new FlagClient({
        apiKey: 'sdk_test_key',
        baseUrl,
        enableRealtime: false,
        enableTelemetry: false,
        defaults: { 'a/b': true, '..': true },
        onError,
      });

    test.each(invalidKeys)('returns the default without a request for key %j', async (key) => {
      const onError = jest.fn();
      const client = makeClient(onError);

      const result = await client.evaluate(key);

      expect(result.reason).toBe('error');
      expect(result.value).toBe(key === 'a/b' || key === '..');
      expect(fetchMock).not.toHaveBeenCalled();
      expect(onError).toHaveBeenCalledTimes(1);
    });

    test('rejects a non-string key without throwing', async () => {
      const onError = jest.fn();
      const client = makeClient(onError);

      const result = await client.evaluate(undefined as any);

      expect(result.reason).toBe('error');
      expect(result.value).toBe(false);
      expect(fetchMock).not.toHaveBeenCalled();
      expect(onError).toHaveBeenCalledTimes(1);
    });

    test('fetches a valid key at one path segment', async () => {
      fetchMock.mockResolvedValue({ ok: true, status: 200, json: async () => ({ key: 'k', enabled: true }) });
      const client = makeClient(jest.fn());

      await client.evaluate('new-checkout.v2_b');

      expect(fetchMock).toHaveBeenCalledTimes(1);
      expect(fetchMock.mock.calls[0][0]).toBe(`${baseUrl}/api/flags/new-checkout.v2_b/evaluate`);
    });

    test('treats ... as a valid key', async () => {
      fetchMock.mockResolvedValue({ ok: true, status: 200, json: async () => ({ key: '...', enabled: true }) });
      const client = makeClient(jest.fn());

      await client.evaluate('...');

      expect(fetchMock).toHaveBeenCalledTimes(1);
      expect(fetchMock.mock.calls[0][0]).toBe(`${baseUrl}/api/flags/.../evaluate`);
    });

    test('accepts a 128-character key', async () => {
      fetchMock.mockResolvedValue({ ok: true, status: 200, json: async () => ({ enabled: true }) });
      const client = makeClient(jest.fn());
      const key = 'a'.repeat(128);

      await client.evaluate(key);

      expect(fetchMock).toHaveBeenCalledTimes(1);
      expect(fetchMock.mock.calls[0][0]).toBe(`${baseUrl}/api/flags/${key}/evaluate`);
    });
  });

  describe('context cache (#127)', () => {
    const baseUrl = 'https://flags.example.test';
    let fetchMock: jest.Mock;
    let originalFetch: typeof global.fetch;

    beforeEach(() => {
      originalFetch = global.fetch;
      fetchMock = jest.fn(async (url: string, init?: { body?: string }) => {
        if (String(url).endsWith('/evaluate')) {
          const ctx = JSON.parse(init!.body as string).context;
          const enabled = ctx.user_id === 'a' || ctx.attributes?.plan === 'pro';
          return { ok: true, status: 200, json: async () => ({ key: 'checkout', enabled }) };
        }
        return {
          ok: true,
          status: 200,
          json: async () => ({
            flags: [{ key: 'checkout', enabled: true, scope: 'application', environments: {}, version: 1 }],
            count: 1,
          }),
        };
      });
      global.fetch = fetchMock as any;
    });

    afterEach(() => {
      global.fetch = originalFetch;
    });

    const makeClient = () =>
      new FlagClient({
        apiKey: 'sdk_test_key',
        baseUrl,
        enableRealtime: false,
        enableTelemetry: false,
      });

    const evalCalls = () => fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/evaluate'));

    test('caches per context argument', async () => {
      const client = makeClient();
      const r = [];
      for (const id of ['a', 'b', 'a', 'b']) {
        r.push(await client.evaluate('checkout', { user_id: id }));
      }
      expect(r.map((x) => x.value)).toEqual([true, false, true, false]);
      expect(r.map((x) => x.reason)).toEqual(['evaluated', 'evaluated', 'cached', 'cached']);
      expect(evalCalls()).toHaveLength(2);
    });

    test('caches per setUserId', async () => {
      const client = makeClient();
      const r = [];
      for (const id of ['a', 'b', 'a', 'b']) {
        client.setUserId(id);
        r.push(await client.evaluate('checkout'));
      }
      expect(r.map((x) => x.value)).toEqual([true, false, true, false]);
      expect(r.map((x) => x.reason)).toEqual(['evaluated', 'evaluated', 'cached', 'cached']);
      expect(evalCalls()).toHaveLength(2);
    });

    test('attributes take part in the key', async () => {
      const client = makeClient();
      const pro = await client.evaluate('checkout', { attributes: { plan: 'pro' } });
      const free = await client.evaluate('checkout', { attributes: { plan: 'free' } });
      expect(pro.value).toBe(true);
      expect(free.value).toBe(false);
      expect(evalCalls()).toHaveLength(2);
    });

    test('attribute insertion order does not matter', async () => {
      const client = makeClient();
      await client.evaluate('checkout', { attributes: { plan: 'pro', region: 'eu' } });
      const second = await client.evaluate('checkout', { attributes: { region: 'eu', plan: 'pro' } });
      expect(second.reason).toBe('cached');
      expect(evalCalls()).toHaveLength(1);
    });

    test('an invalidation during the request drops its result (#137)', async () => {
      const client = makeClient();
      let n = 0;
      fetchMock.mockImplementation(async () => {
        n++;
        const enabled = n === 1;
        // The kill switch flips mid-request (a stream message invalidates the flag).
        if (n === 1) (client as any).cache.invalidate('checkout');
        return { ok: true, status: 200, json: async () => ({ key: 'checkout', enabled }) };
      });
      const first = await client.evaluate('checkout', { user_id: 'a' });
      const second = await client.evaluate('checkout', { user_id: 'a' });
      expect(first.value).toBe(true);
      expect(second.reason).toBe('evaluated');
      expect(second.value).toBe(false);
      expect(evalCalls()).toHaveLength(2);
    });

    test('clearCache during the request drops its result (#137)', async () => {
      const client = makeClient();
      let n = 0;
      fetchMock.mockImplementation(async () => {
        n++;
        if (n === 1) client.clearCache();
        return { ok: true, status: 200, json: async () => ({ key: 'checkout', enabled: n === 1 }) };
      });
      await client.evaluate('checkout', { user_id: 'a' });
      const second = await client.evaluate('checkout', { user_id: 'a' });
      expect(second.reason).toBe('evaluated');
      expect(second.value).toBe(false);
    });

    test('clearCache makes every context re-request', async () => {
      const client = makeClient();
      await client.evaluate('checkout', { user_id: 'a' });
      await client.evaluate('checkout', { user_id: 'b' });
      client.clearCache();
      await client.evaluate('checkout', { user_id: 'a' });
      await client.evaluate('checkout', { user_id: 'b' });
      expect(evalCalls()).toHaveLength(4);
    });

    test('getAllFlags does not seed the evaluation cache', async () => {
      const client = makeClient();
      const flags = await client.getAllFlags('production');
      expect(flags).toHaveLength(1);
      // The list's `enabled` is the environment switch, not a context's result.
      expect(client.getCachedFlags()).toEqual([]);
      const result = await client.evaluate('checkout', { user_id: 'b' });
      expect(result.value).toBe(false);
      expect(result.reason).toBe('evaluated');
    });
  });
});
