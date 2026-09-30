# Changelog

## Unreleased

### Added

- Add durable operational audit rows, fenced delivery claims, accepted-message recovery, quarantine and a persistent delivery halt to the sqlx core store. Reserve audit migrations 0340–0349 and run isolated Postgres regressions in CI; Discord service/runtime activation remains a follow-up. Every owner mutation takes the audit row lock before evaluating token, generation and the current lease, and preflight-only failures park rows out of queue discovery for a 60 s backoff without counting a POST attempt. Deferral and failure releases stamp a fairness yield fixing queue position at release time, so expired backoffs rotate past repeatedly failing rows while aged retries keep position ahead of later arrivals instead of being starved by continued fresh rows, and a lock-free eligibility precheck keeps ineligible claims from waiting on a row lock while holding the shared halt guard.
- Port the website-to-bot internal-actions auth core (HMAC-SHA256 rotation-aware signing, skew + nonce replay guard, post-verify token buckets, 19-verb allowlist with env flags, settings catalog guard, bind guard, `authorize` pipeline) as framework-free domain logic. HTTP route, durable stores, and Discord execution remain follow-up slices.
- Framework-free LFG role parsing, future start-time validation, signup capacity decisions, select-menu data, permission checks, and message rendering.
- PostgreSQL LFG persistence and migration `0170`, preserving legacy table and column names; guild-fenced post writes and serialized capacity/close transactions.
- LFG regression tests against an isolated PostgreSQL CI service. Runtime command/component registration and Discord side effects remain dependent on the S4 interaction-router and REST-executor slices.
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
- Channel moderation domain and SQL store for purge bounds, slowmode bounds,
  exact lockdown overwrite recovery, refusal of unlock without recorded state,
  generation-fenced idempotency claims and audit rows. Router/REST execution wiring follows when
  the shared S4 seams are merged.
- Fence lockdown recovery cleanup to the generation that was restored, so a delayed
  unlock (or a retried cleanup whose earlier result was lost) reports stale instead of
  deleting a later lockdown cycle's seed. Repeated lockdowns preserve the original
  generation alongside the original seed (migration 0122).
- Member moderation domain for ban, tempban, kick, timeout and warn, with
  idempotent claims, durable scheduled-unban recovery and audit/warning ledgers.
- Feature-gated Postgres moderation store and isolated test-container CI coverage.

### Fixed

- Match legacy ECMAScript whitespace trimming for LFG roles, slot numbers, and titles, including BOM and NEL edge cases.
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
- Scope unban recovery to its guild, fence older expiries with durable ban
  generations, and recover only explicitly accepted bans rather than guessed
  staging. Permanent bans supersede older tempbans; failed refusal cleanup keeps
  a reconciliation fence instead of scheduling an unsafe unban.
- Claim sweep jobs individually so cancellation cannot strand an undispatched
  batch, and bound the final generated expiry reason with Unicode-safe truncation.

### Notes

- Command/component wiring and the Discord REST reads stay on the S4 interaction router and REST executor slices; the outcome enums are the integration surface until they land. Scorecard and probe collection are not enabled by this change.
- Runtime registration, the shared REST executor's 5-second abort and the
  30-second scheduler remain gated on the S4 integration slices; moderation
  is not enabled by this change.
