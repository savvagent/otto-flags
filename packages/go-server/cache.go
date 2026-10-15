package ottoflags

import (
	"sync"
	"time"
)

// maxContextsPerFlag bounds how many contexts the cache holds for one flag, so a host that
// evaluates a flag for many users cannot grow memory without limit.
const maxContextsPerFlag = 10000

// cache provides thread-safe in-memory caching for flag values. Entries are held per flag and
// then per context (the JSON of the evaluation context), because the server's answer depends on
// the context.
type cache struct {
	mu                 sync.RWMutex
	entries            map[string]map[string]cacheEntry
	ttl                time.Duration
	maxContextsPerFlag int
}

// newCache creates a new cache with the specified TTL
func newCache(ttl time.Duration) *cache {
	return &cache{
		entries:            make(map[string]map[string]cacheEntry),
		ttl:                ttl,
		maxContextsPerFlag: maxContextsPerFlag,
	}
}

// Get retrieves a cached value for the flag and context if it exists and hasn't expired
func (c *cache) Get(key, ctxKey string) (bool, bool) {
	entry, found := c.GetEntry(key, ctxKey)
	if !found {
		return false, false
	}
	return entry.Value, true
}

// GetEntry retrieves a complete cached entry (includes configuration and variation)
func (c *cache) GetEntry(key, ctxKey string) (*cacheEntry, bool) {
	c.mu.RLock()
	defer c.mu.RUnlock()

	entry, exists := c.entries[key][ctxKey]
	if !exists {
		return nil, false
	}

	// Check if expired
	if time.Now().After(entry.ExpiresAt) {
		return nil, false
	}

	return &entry, true
}

// Set stores a value in the cache with TTL. When the flag already holds the maximum number of
// contexts, expired entries are dropped first, and if more than half the bound is still live
// the flag's contexts are cleared, so a sweep runs at most once per half-bound of new contexts.
func (c *cache) Set(key, ctxKey string, value bool, configuration map[string]interface{}, variation string) {
	c.mu.Lock()
	defer c.mu.Unlock()

	contexts, ok := c.entries[key]
	if !ok {
		contexts = make(map[string]cacheEntry)
		c.entries[key] = contexts
	}
	if _, present := contexts[ctxKey]; !present && len(contexts) >= c.maxContextsPerFlag {
		now := time.Now()
		for k, e := range contexts {
			if now.After(e.ExpiresAt) {
				delete(contexts, k)
			}
		}
		if len(contexts) > c.maxContextsPerFlag/2 {
			for k := range contexts {
				delete(contexts, k)
			}
		}
	}

	contexts[ctxKey] = cacheEntry{
		Value:         value,
		Configuration: configuration,
		Variation:     variation,
		ExpiresAt:     time.Now().Add(c.ttl),
	}
}

// Invalidate removes every context cached for a flag
// If key is empty, it clears all entries
func (c *cache) Invalidate(key string) {
	c.mu.Lock()
	defer c.mu.Unlock()

	if key == "" {
		c.entries = make(map[string]map[string]cacheEntry)
	} else {
		delete(c.entries, key)
	}
}

// Clear removes all entries from the cache
func (c *cache) Clear() {
	c.Invalidate("")
}
