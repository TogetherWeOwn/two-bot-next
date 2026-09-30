# Changelog

## Unreleased

### Added

- Automod gateway decision/enrichment seams and capture-only funnel handoff,
  staging/live-approval and dry-run fences, protected-target enforcement plans,
  repeat-history expiration, and replay-safe delivery claims with the
  legacy-compatible once-per-message violation ledger. Shared executor/shard
  activation is not enabled by this slice. (TOG-10089)
- Port leveling XP awards and per-source cooldowns to a transactional sqlx store over the imported tables, with legacy rank/leaderboard replies, idempotent reward-role plans and isolated Postgres parity tests. Router, REST and async gateway wiring remain follow-up integration work.
- Presence probe, weekly community scorecard, and inactivity flagging as framework-free domain logic with feature-gated Postgres stores and migrations 0310–0311, verified against a golden scorecard from the legacy build: hourly presence series with 24 h bot-floor re-list and the reopen trigger, Monday 06:15 UTC closed-week runs with fail-closed coverage, and an hourly read-only quiet-member sweep that never messages.
- Port join-burst detection, join-risk scoring and mention-suppressed staff alert proposals to the Rust domain core, with occurrence/processing clock boundaries and mock acceptance. No gateway, durable store, alert delivery or anti-nuke activation is added.
- Port game/session picker decisions, legacy/session/anchor welcome modes, rules-gate prompt eligibility and mention-free session goodbyes to the Rust domain core. Preserve legacy funnel rows with a sqlx prompt guard, migration 0190 and isolated agent-testdb/mock delivery tests. Runtime router/REST wiring remains a follow-up.
- Port sticky-message domain logic, debounce claims and PostgreSQL persistence, with a legacy timestamp upgrade and UTF-16-compatible body limits. Discord command and REST wiring remains in the S4 integration slices.
- RSVP domain logic and database store for going, interested and declined responses, namespaced attendance totals, and ManageEvents-gated host check-in facts, with legacy-compatible tables and replies.
- Port website-contract counter, rank and scheduled-events domain logic and transactional storage, with legacy-shaped read views. Runtime job wiring remains deferred.
- Port the guild-settings catalogue, 15-second poll contract, cache, sqlx store, and migration to Rust. Runtime ticker and interaction integration remain follow-up work.
- Run isolated settings persistence and concurrency regressions against a disposable Postgres CI service without credentials.
- Persist gateway session, resume URL and processed sequence across Container restarts. Commit funnel rows and checkpoints atomically, restore message milestones, discard stale sessions, and fall back to IDENTIFY when Discord invalidates a session.

### Fixed

- Detect settings changes with a commit-ordered transactional revision instead of a sequence maximum, including deletes and late commits with lower row versions.
- Serialize settings reads, writes, and audits, including concurrent inserts into absent keys; load cache rows and revision from one consistent database snapshot.
- Refuse environment-only and unknown settings through the cache getter as well as environment snapshots.
- Enforce append-only settings audit data for updates, deletes, and truncation.
- Render integral JSON settings as integer environment strings so decimal/exponent thresholds survive database round-trips into config readers, without rounding integer IDs through floating point.
- Serialize concurrent first RSVP responses before reading the previous status, including when no response row exists yet.
- Fail configured RSVP database-test setup errors instead of silently skipping, and isolate each test invocation in its own schema.
- Preserve isolated bot schemas when applying the website contract, without rebinding the public read views.
- Refuse non-test targets before resetting the website-contract acceptance database.
- Reject malformed scheduled-event timestamps without panicking or replacing the last good mirror.
- Recover gateway sessions rejected with close codes 4007/4009, preserve the committed READY URL after endpoint fallback, and exit for Container restart when the essential gateway task stops instead of serving a healthy zombie.
- Bound total checkpoint SQL waits to a heartbeat-safe deadline and report readiness unavailable while persistence is pending; fail closed and restore from committed state after a slow-database restart.

### Notes

- Command/component wiring and the Discord REST reads stay on the S4 interaction router and REST executor slices; the outcome enums are the integration surface until they land. Scorecard and probe collection are not enabled by this change.
