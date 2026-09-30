# Changelog

## Unreleased

### Added

- Channel moderation domain and SQL store for purge bounds, slowmode bounds,
  exact lockdown overwrite recovery, refusal of unlock without recorded state,
  generation-fenced idempotency claims and audit rows. Router/REST execution wiring follows when
  the shared S4 seams are merged.
- RSVP domain logic and database store for going, interested and declined responses, namespaced attendance totals, and ManageEvents-gated host check-in facts, with legacy-compatible tables and replies.

### Fixed

- Serialize concurrent first RSVP responses before reading the previous status, including when no response row exists yet.
- Fail configured RSVP database-test setup errors instead of silently skipping, and isolate each test invocation in its own schema.
