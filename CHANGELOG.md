# Changelog

## Unreleased

### Added

- Scheduled-message domain logic, PostgreSQL store and migration, with validation, prefix-resolved removal, recurring timing and retry outcomes. Discord router/executor wiring follows separately.
- Scheduled-store integration tests run against a PostgreSQL service container in CI.

### Fixed

- Scheduled claims lease one occurrence per call, preventing distinct messages from sharing a batch nonce while preserving the nonce across retries and restarts.
- Scheduled-message ID prefixes treat `%`, `_` and backslashes literally, matching the domain resolver.
- Configured scheduled-store tests fail on connection, migration or cleanup errors instead of silently skipping; test URLs are restricted to test databases.
