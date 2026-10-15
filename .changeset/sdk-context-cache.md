---
"@otto-flags/sdk": patch
---

Cache each evaluation per flag and evaluation context, so one context's result is never returned for another; `getAllFlags` no longer fills the evaluation cache.
