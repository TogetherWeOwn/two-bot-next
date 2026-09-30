# Changelog

## Unreleased

### Added

- Framework-free LFG role parsing, future start-time validation, signup capacity decisions, select-menu data, permission checks, and message rendering.
- PostgreSQL LFG persistence and migration `0170`, preserving legacy table and column names; guild-fenced post writes and serialized capacity/close transactions.
- LFG regression tests against an isolated PostgreSQL CI service. Runtime command/component registration and Discord side effects remain dependent on the S4 interaction-router and REST-executor slices.
