# Changelog

## Unreleased

### Added

- Framework-free LFG role parsing, future start-time validation, signup capacity decisions, select-menu data, permission checks, and message rendering.
- PostgreSQL LFG persistence and migration `0170`, preserving legacy table and column names; guild-fenced post writes and serialized capacity/close transactions.
- LFG regression tests against an isolated PostgreSQL CI service. Runtime command/component registration and Discord side effects remain dependent on the S4 interaction-router and REST-executor slices.
- Port sticky-message domain logic, debounce claims and PostgreSQL persistence, with a legacy timestamp upgrade and UTF-16-compatible body limits. Discord command and REST wiring remains in the S4 integration slices.
- RSVP domain logic and database store for going, interested and declined responses, namespaced attendance totals, and ManageEvents-gated host check-in facts, with legacy-compatible tables and replies.

### Fixed

- Match legacy ECMAScript whitespace trimming for LFG roles, slot numbers, and titles, including BOM and NEL edge cases.
- Serialize concurrent first RSVP responses before reading the previous status, including when no response row exists yet.
- Fail configured RSVP database-test setup errors instead of silently skipping, and isolate each test invocation in its own schema.
