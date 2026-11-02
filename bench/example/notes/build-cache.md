---
name: build-cache
description: CI build cache keying and the toolchain-bump gotcha
metadata:
  type: project
---
- The CI cache is keyed on the lockfile hash; a full build without it takes ~14 minutes.
- **Gotcha:** after a compiler toolchain bump, stale cache entries cause link errors. Clear the cache manually.
