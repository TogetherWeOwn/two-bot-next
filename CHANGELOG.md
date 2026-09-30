# Changelog

## [Unreleased]

### Added

- Member moderation domain for ban, tempban, kick, timeout and warn, with
  idempotent claims, durable scheduled-unban recovery and audit/warning ledgers.
- Feature-gated Postgres moderation store and isolated test-container CI coverage.
- Port sticky-message domain logic, debounce claims and PostgreSQL persistence, with a legacy timestamp upgrade and UTF-16-compatible body limits. Discord command and REST wiring remains in the S4 integration slices.
- RSVP domain logic and database store for going, interested and declined responses, namespaced attendance totals, and ManageEvents-gated host check-in facts, with legacy-compatible tables and replies.

### Fixed

- Scope unban recovery to its guild, fence older expiries with durable ban
  generations, and recover only explicitly accepted bans rather than guessed
  staging. Permanent bans supersede older tempbans; failed refusal cleanup keeps
  a reconciliation fence instead of scheduling an unsafe unban.
- Claim sweep jobs individually so cancellation cannot strand an undispatched
  batch, and bound the final generated expiry reason with Unicode-safe truncation.
- Serialize concurrent first RSVP responses before reading the previous status, including when no response row exists yet.
- Fail configured RSVP database-test setup errors instead of silently skipping, and isolate each test invocation in its own schema.

### Notes

- Runtime registration, the shared REST executor's 5-second abort and the
  30-second scheduler remain gated on the S4 integration slices; moderation
  is not enabled by this change.
