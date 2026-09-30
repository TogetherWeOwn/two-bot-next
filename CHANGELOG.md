# Changelog

## [Unreleased]

### Added

- Presence probe, weekly community scorecard, and inactivity flagging as
  framework-free domain logic with feature-gated Postgres stores and
  migrations 0310–0311, verified against a golden scorecard from the legacy
  build: hourly presence series with 24 h bot-floor re-list and the reopen
  trigger, Monday 06:15 UTC closed-week runs with fail-closed coverage, and
  an hourly read-only quiet-member sweep that never messages.

### Notes

- Command/component wiring and the Discord REST reads stay on the S4
  interaction router and REST executor slices; the outcome enums are the
  integration surface until they land. Scorecard and probe collection are not
  enabled by this change.
