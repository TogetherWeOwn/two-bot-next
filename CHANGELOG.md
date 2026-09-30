# Changelog

## Unreleased

### Added

- Port game/session picker decisions, legacy/session/anchor welcome modes, rules-gate prompt eligibility and mention-free session goodbyes to the Rust domain core. Preserve legacy funnel rows with a sqlx prompt guard, migration 0190 and isolated agent-testdb/mock delivery tests. Runtime router/REST wiring remains a follow-up.
- Port sticky-message domain logic, debounce claims and PostgreSQL persistence, with a legacy timestamp upgrade and UTF-16-compatible body limits. Discord command and REST wiring remains in the S4 integration slices.
- RSVP domain logic and database store for going, interested and declined responses, namespaced attendance totals, and ManageEvents-gated host check-in facts, with legacy-compatible tables and replies.
- Port the guild-settings catalogue, 15-second poll contract, cache, sqlx store, and migration to Rust. Runtime ticker and interaction integration remain follow-up work.
- Run isolated settings persistence and concurrency regressions against a disposable Postgres CI service without credentials.
- Member moderation domain for ban, tempban, kick, timeout and warn, with
  idempotent claims, durable scheduled-unban recovery and audit/warning ledgers.
- Feature-gated Postgres moderation store and isolated test-container CI coverage.

### Fixed

- Detect settings changes with a commit-ordered transactional revision instead of a sequence maximum, including deletes and late commits with lower row versions.
- Serialize settings reads, writes, and audits, including concurrent inserts into absent keys; load cache rows and revision from one consistent database snapshot.
- Refuse environment-only and unknown settings through the cache getter as well as environment snapshots.
- Enforce append-only settings audit data for updates, deletes, and truncation.
- Render integral JSON settings as integer environment strings so decimal/exponent thresholds survive database round-trips into config readers, without rounding integer IDs through floating point.
- Serialize concurrent first RSVP responses before reading the previous status, including when no response row exists yet.
- Fail configured RSVP database-test setup errors instead of silently skipping, and isolate each test invocation in its own schema.
- Scope unban recovery to its guild, fence older expiries with durable ban
  generations, and recover only explicitly accepted bans rather than guessed
  staging. Permanent bans supersede older tempbans; failed refusal cleanup keeps
  a reconciliation fence instead of scheduling an unsafe unban.
- Claim sweep jobs individually so cancellation cannot strand an undispatched
  batch, and bound the final generated expiry reason with Unicode-safe truncation.

### Notes

- Runtime registration, the shared REST executor's 5-second abort and the
  30-second scheduler remain gated on the S4 integration slices; moderation
  is not enabled by this change.
