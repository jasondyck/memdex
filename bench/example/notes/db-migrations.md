---
name: db-migrations
description: How schema migrations are run and the rule about editing them
metadata:
  type: project
---
- Migrations run in the deploy job before the app starts; the staging database is at 203.0.113.7:5432.
- **Never edit a migration that has already been applied**; add a new one instead.
