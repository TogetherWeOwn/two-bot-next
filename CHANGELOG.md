# Changelog

## [Unreleased]

### Added

- Member moderation domain for ban, tempban, kick, timeout and warn, with
  idempotent claims, durable scheduled-unban recovery and audit/warning ledgers.
- Feature-gated Postgres moderation store and isolated test-container CI coverage.

### Notes

- Runtime registration, the shared REST executor's 5-second abort and the
  30-second scheduler remain gated on the S4 integration slices; moderation
  is not enabled by this change.
