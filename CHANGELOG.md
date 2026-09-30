# Changelog

## Unreleased

### Added

- Port game/session picker decisions, legacy/session/anchor welcome modes, rules-gate prompt eligibility and mention-free session goodbyes to the Rust domain core. Preserve legacy funnel rows with a sqlx prompt guard, migration 0190 and isolated agent-testdb/mock delivery tests. Runtime router/REST wiring remains a follow-up.
- RSVP domain logic and database store for going, interested and declined responses, namespaced attendance totals, and ManageEvents-gated host check-in facts, with legacy-compatible tables and replies.

### Fixed

- Serialize concurrent first RSVP responses before reading the previous status, including when no response row exists yet.
- Fail configured RSVP database-test setup errors instead of silently skipping, and isolate each test invocation in its own schema.
