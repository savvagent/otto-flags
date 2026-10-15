/**
 * Canonical cache key for an evaluation context.
 *
 * The evaluation cache is keyed by flag and then by the context the SDK sends, so one
 * context's result is never served for another (#127). The key is the JSON of the context
 * with every object's keys sorted: two contexts that serialize to the same request body
 * share a key whatever order their attributes were inserted in. `undefined` fields are
 * dropped (as the request body drops them) and `toJSON` is applied.
 *
 * Returns `null` when the context cannot be serialized (a cycle, a BigInt, a throwing
 * `toJSON`); the caller must then skip the cache for that call and evaluate remotely.
 */
export function contextKey(value: unknown): string | null {
  try {
    const first = JSON.stringify(value);
    if (first === undefined) return null;
    return JSON.stringify(sortKeys(JSON.parse(first)));
  } catch {
    return null;
  }
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value !== null && typeof value === 'object') {
    const out: Record<string, unknown> = {};
    for (const k of Object.keys(value as object).sort()) {
      out[k] = sortKeys((value as Record<string, unknown>)[k]);
    }
    return out;
  }
  return value;
}
