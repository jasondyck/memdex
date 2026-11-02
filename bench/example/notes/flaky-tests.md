---
name: flaky-tests
description: Known flaky test and its fix
metadata:
  type: feedback
---
- `test_checkout_timeout` failed randomly because it used the real clock.
- Fixed by injecting a fake clock; never sleep in tests.
