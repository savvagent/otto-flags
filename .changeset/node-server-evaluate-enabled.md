---
"@otto-flags/node-server": patch
---

Read the evaluation result from `enabled`, the field the server sends, instead of `value`. A response without a boolean `enabled` is now a failed lookup that returns the default and is not cached.
