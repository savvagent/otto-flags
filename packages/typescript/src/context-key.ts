/**
 * Canonical cache key for an evaluation context.
 *
 * The evaluate endpoint answers from the flag key, the API key and the request body's context, so
 * a cached answer is only valid for the same context. The key is the context's JSON with object
 * keys sorted at every depth, so attribute insertion order does not change it. `undefined` fields
 * are dropped, as they are when the request body is serialized.
 *
 * Returns `null` when the context cannot be serialized (a cycle, a BigInt, a throwing `toJSON`);
 * the caller then skips the cache.
 */
export function contextKey(value: unknown): string | null {
  try {
    const json = JSON.stringify(value);
    if (json === undefined) {
      return null;
    }
    return JSON.stringify(sortKeys(JSON.parse(json)));
  } catch {
    return null;
  }
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(sortKeys);
  }
  if (value !== null && typeof value === 'object') {
    const source = value as Record<string, unknown>;
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(source).sort()) {
      sorted[key] = sortKeys(source[key]);
    }
    return sorted;
  }
  return value;
}
