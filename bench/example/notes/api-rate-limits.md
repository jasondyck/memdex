---
name: api-rate-limits
description: Upstream API quota and the retry policy
metadata:
  type: project
---
- The upstream API allows 100 requests per minute per key.
- On HTTP 429, back off exponentially starting at 2 seconds.
