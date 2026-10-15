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
});
