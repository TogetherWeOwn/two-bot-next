# Changelog

## Unreleased

### Added

- **Self-role domain and storage:** framework-free button/select/reaction plans,
  configuration and live-role safety validation, hierarchy refusals, and
  legacy-compatible audit/panel tables (migration 0200). Shared event and
  exclusive-panel leases support renewal, expiry recovery, immutable mutation
  intent, fencing, and atomic audit/target settlement. The isolated Postgres
  lease regression test runs in CI and gates the required `check` job.
  Runtime router/REST wiring remains deferred; this does not enable Discord
  role mutations.
- RSVP domain logic and database store for going, interested and declined responses, namespaced attendance totals, and ManageEvents-gated host check-in facts, with legacy-compatible tables and replies.

### Fixed

- Serialize concurrent first RSVP responses before reading the previous status, including when no response row exists yet.
- Fail configured RSVP database-test setup errors instead of silently skipping, and isolate each test invocation in its own schema.
