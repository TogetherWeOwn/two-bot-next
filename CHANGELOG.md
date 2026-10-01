# Changelog

## Unreleased

### Added

- Add self-role reads and singular role operations to the shared REST executor:
  authoritative member/bot/role/channel policy snapshots, fetched reaction-message
  identity, paced single-attempt operations with pre/post ownership checks and
  retained ambiguous or accepted-but-stale exchanges. Add event-first/lane-first
  runtime admission, concurrent cancellation-safe renewal of both leases, and
  one-time fenced intent initialization (migration 0201). Recovery preserves
  intentionally empty snapshots and computes remaining work from freshly fetched
  member state. Add durable paced send journaling, fresh-policy singular execution,
  effect checkpoints, and a monotonic compensation phase (migration 0202) that
  survives restart without retrying rejected intent. Add newly leased stale-worker
  repair to the committed target, live atomic runtime settlement, and durable
  pending-exchange recovery (migration 0203) that refuses false success after an
  interrupted remote send. Add isolated Postgres/mock REST regressions for
  partial/ambiguous failures, compensation recovery, stale in-flight repair,
  selected/empty targets, expiry rollback and unresolved settlement refusal.
  Add injectable shared component/reaction dispatch with source and input-type
  validation, ephemeral defer-before-admission and final-settlement-only success
  replies. Add fenced dry-run audits without role mutations or simulated target
  publication, plus input and orchestration regressions. Add bounded configured-
  source discovery and generation-fenced recovery of expired processing audits
  without gateway redelivery, preserving initialized empty targets and unresolved
  evidence. Gate reaction dispatch through the shared router and add actual
  partial/duplicate add/remove fixtures. Add optional shared-supervisor recovery
  registration with bounded cadence/timeout and discovery I/O, per-name parked
  status, and cancellation/evidence-preservation fixtures. Add separate terminal
  supersession discovery, fresh typed evidence leases, dual-fenced repair
  journaling/completion receipts and inherited-unknown preservation (migration
  0204), with isolated source regressions. Add cancellation-owned terminal restart
  repair through the shared journaled executor to a freshly leased committed
  selected/empty target, atomic paired ownership checks and bounded fair mixed
  discovery. Preserve inherited uncertainty and terminal rejection; dry-run skips
  terminal claims. Add source fixtures for restart convergence, missing/unknown
  targets, pending evidence, cancelled renewals and mixed sweeps. Extend terminal
  fault-injection source coverage for partial repair rejection/rate-limit/received
  ambiguity/timeout, independent evidence/lane transfer during REST, and expiry
  after pacing or journal waits without false sends or uncertainty retirement.
  Route older processing audits inserted after the winning lane's bulk
  supersession into terminal discovery before prepared ownership, using a fresh
  post-lock event fence and stored scope/chronology checks. Preserve all intent,
  pending and compensation evidence without REST or winner publication; add
  early-supersession runtime/store and lock-wait source fixtures. Preserve singular
  role-response status before unused provider-body reads can fail or stall, while
  requiring complete snapshot bodies. Resolve definite new role/direction evidence
  without erasing older unknown sends or mislabeling unrelated acknowledged work;
  add partial-body, ownership-loss and inherited-evidence source regressions.
  Add distinct per-send journal tickets and idempotent response/no-send receipts
  (migration 0205) that survive generation transfer without former-worker audit
  or target authority. Pending tickets gate settlement and cannot be erased by
  aggregate checkpoint clearing. Read ticket evidence only after audit lock waits
  and preserve unresolved role/direction IDs from pending tickets. Add processing/
  terminal provenance, lock-wait and role-matrix source fixtures. Wire tickets
  into normal processing and typed terminal paced runtime steps, persisting raw
  response/no-send receipts before stale aggregate writes; retain timeout and
  cancellation uncertainty without retry. Extend runtime/executor source fixtures
  for status provenance, generation transfer, post-journal no-send and pending
  tickets. Add live-fenced current-owner receipt evidence incorporation to processing
  recovery and typed terminal repair, restoring cumulative attempts and acknowledged
  204 compensation while preserving pending/legacy uncertainty, snapshot effects,
  terminal outcome and committed target. Distinguish received ambiguous effects
  from unknown in-flight sends. Add replay, stale-fence, lock-wait and recovery
  source coverage. Receipt-specific uncertainty attribution/retirement and migration
  of the legacy lane-only stale-maintenance journal remain deferred.
  Production boot remains disabled until approved shared boot configuration,
  unresolved-work continuation and compiled acceptance are complete.
- **Self-role domain and storage:** framework-free button/select/reaction plans,
  configuration and live-role safety validation, hierarchy refusals, and
  legacy-compatible audit/panel tables (migration 0200). Shared event and
  exclusive-panel leases support renewal, expiry recovery, immutable mutation
  intent, fencing, and atomic audit/target settlement. Lease checks use database
  wall time after lock waits; recovery preserves cumulative attempted/compensated
  evidence. Generated events have cross-worker tie-breakers, and controls enforce
  UTF-16 and Discord size limits. The isolated Postgres lease regression test
  runs in CI and gates the required `check` job.
  Runtime router/REST wiring remains deferred; this does not enable Discord
  role mutations.
- Wire `/feed-add`, `/feed-remove` and `/feed-list` through the same command runtime, router and REST executor as sticky commands. Defer ephemerally before guild-scoped CRUD and audit writes, preserve the invoking channel, and publish the complete gated command registry on Ready. Isolated Postgres and mock REST acceptance cover feed commands and sticky coexistence; announcements remain off by default, with no fetching, polling or relay posts.
- Wire `/sticky` and `/sticky-remove` through the shared interaction router and drive accepted-message re-posts through the shared REST executor. The runtime claims one re-post window atomically per burst, validates and records the replacement id before retiring the previous sticky (best-effort), releases the claim and audits `post_failed` on REST failure, and deletes an orphaned replacement when the claim moved on. Commands defer ephemerally before I/O and edit the original reply; refusals are limited to owned sticky commands. `TWO_AUTOMATIONS` gates both surfaces; ManageGuild is required, and `/sticky` validates a 1–2000 UTF-16 body with a 1–300 s debounce (default 5 s). Covered by isolated Postgres and mock REST acceptance tests.
- Add the audit mirror delivery service over the shared REST executor: record-before-deliver, private guild-fenced destinations, enforced nonce/mention suppression, kill-switch enforcement, crash-safe marker reconciliation and quarantine. Revalidate prepared ownership after shared transport pacing, preserve interrupted dedup adoption evidence, treat malformed history as uncertain, and run service fault-injection regressions in CI. Runtime wiring and activation remain deferred.
- Add the pinned-address HTTPS feed connector (`feeds_connector`). `fetch_feed` validates the source through `feeds_http`, resolves all A/AAAA answers once, pins them into a `PublicRequest`, and dials only those addresses through a per-request hyper/rustls client whose resolver answers only the pinned host. TLS SNI, certificate verification and the Host header keep the URL hostname; every redirect hop re-validates and re-pins under a three-hop, same-host, HTTPS-only budget, one 15 s total deadline, `Owen/1.0 (+https://two.gg)` identity, and compressed-then-decompressed `MAX_FEED_BYTES` bounds before XML parsing. Injected resolver/connector seams keep every test hermetic; the private-fixture constructor exists only under `#[cfg(test)]`.

### Fixed

- Answer published but unwired commands ephemerally instead of timing out, preserving the existing router refusals and complete registry. Synchronize the registry on resumed process startup as well as Ready, without blocking gateway polling.
- Capture the REST pacing timestamp after the lane wait completes, preserving adjacent-request spacing across three or more reads and kicks.
- Grant the least-privilege runtime role CRUD on the self-role audit and panel-claim relations (migration 0200), cover 0200 in the role-matrix tests, and prove runtime claim access with continued web-reader denial.
- Record late result/compensation evidence for superseded self-role events under their still-current token/generation without reopening settlement or panel publication, with regression coverage.
- Reject the guild @everyone role as a self-role mutation target during catalogue validation and unconditionally at dispatch.
- Hold self-role claim fencing tokens in `Secret` so derived `Debug` redacts them; the raw value is exposed only at the SQL fencing comparisons.

## [0.2.0](https://github.com/TogetherWeOwn/two-bot-next/releases/tag/v0.2.0) (2026-09-30)

### Added

* **audit:** add durable fenced delivery storage ([#58](https://github.com/TogetherWeOwn/two-bot-next/issues/58)) ([4f8c460](https://github.com/TogetherWeOwn/two-bot-next/commit/4f8c4606fbdc9256a88888c740168057eecbad3c))
* **audit:** port operational audit classifiers and moderation MAC ([#29](https://github.com/TogetherWeOwn/two-bot-next/issues/29)) ([58b5184](https://github.com/TogetherWeOwn/two-bot-next/commit/58b51843fae9fed08052d3d5a7dee0e4b28155b8))
* **backup:** port dump/restore, S3 upload, sealed guild-config snapshot ([#11](https://github.com/TogetherWeOwn/two-bot-next/issues/11)) ([4cb4733](https://github.com/TogetherWeOwn/two-bot-next/commit/4cb47339b58d8e9c320d147ebd9e1a8bb7f47c31))
* **bot:** B2 container hardening, staging deploy, soak runbook ([#4](https://github.com/TogetherWeOwn/two-bot-next/issues/4)) ([63fbdb1](https://github.com/TogetherWeOwn/two-bot-next/commit/63fbdb1130eee22d3db5f376bd49cd1cf8b3cc61))
* **commands:** automod matcher plus sanctions and env gates ([#14](https://github.com/TogetherWeOwn/two-bot-next/issues/14)) ([0129b58](https://github.com/TogetherWeOwn/two-bot-next/commit/0129b58962a4b739141d58adbc9db503311882a0))
* **commands:** feature command shapes plus env gates ([#10](https://github.com/TogetherWeOwn/two-bot-next/issues/10)) ([84809c1](https://github.com/TogetherWeOwn/two-bot-next/commit/84809c1ee77b933f5565b5b63495b94dfae2254a))
* **commands:** moderation shapes plus policy and env gates ([#12](https://github.com/TogetherWeOwn/two-bot-next/issues/12)) ([16d1f11](https://github.com/TogetherWeOwn/two-bot-next/commit/16d1f113830f975bc8b16e79ca5d9a41e5695103))
* **commands:** registry merge plus leveling domain port ([#9](https://github.com/TogetherWeOwn/two-bot-next/issues/9)) ([d0d4a36](https://github.com/TogetherWeOwn/two-bot-next/commit/d0d4a36e2ecc26c6ebbd736e12bae5d2a31ff669))
* **community:** presence probe, weekly scorecard, inactivity flagging ([#46](https://github.com/TogetherWeOwn/two-bot-next/issues/46)) ([2dc5e7f](https://github.com/TogetherWeOwn/two-bot-next/commit/2dc5e7f9bb97910773818b3013ca8008dc41dca7))
* **containment:** port pure anti-nuke policy and quarantine planner ([#44](https://github.com/TogetherWeOwn/two-bot-next/issues/44)) ([d8f46e4](https://github.com/TogetherWeOwn/two-bot-next/commit/d8f46e4afdbb9b90368a5160cd7d5d6b1087bc30))
* **cutover:** port MEE6 XP, rewards, history backfill operator tools ([#13](https://github.com/TogetherWeOwn/two-bot-next/issues/13)) ([fe20bdf](https://github.com/TogetherWeOwn/two-bot-next/commit/fe20bdf1d7cfcd9bf738d1d588e42581cb5ff08e))
* **discord:** REST action executor with legacy pacing plus mock REST double ([#63](https://github.com/TogetherWeOwn/two-bot-next/issues/63)) ([e12aee0](https://github.com/TogetherWeOwn/two-bot-next/commit/e12aee051a37bab72f389bc5ddad2ffe2f1ea853))
* **gateway:** event pipeline plus funnel port ([#15](https://github.com/TogetherWeOwn/two-bot-next/issues/15)) ([173146e](https://github.com/TogetherWeOwn/two-bot-next/commit/173146ee9d85b470bcdd41f60a1a2f5f3c6a8b6d))
* **gateway:** persist checkpoints and resume across restarts ([#26](https://github.com/TogetherWeOwn/two-bot-next/issues/26)) ([6c47742](https://github.com/TogetherWeOwn/two-bot-next/commit/6c477427348b4515df2c757129cddbeea2e895ca))
* **internal-actions:** add durable replay and execution store ([#61](https://github.com/TogetherWeOwn/two-bot-next/issues/61)) ([751499e](https://github.com/TogetherWeOwn/two-bot-next/commit/751499e988a3c1b0b4c83c32ad2376bf5d0a14de))
* **internal-actions:** port signing, replay guard, buckets, allowlist validators ([#17](https://github.com/TogetherWeOwn/two-bot-next/issues/17)) ([dda04e5](https://github.com/TogetherWeOwn/two-bot-next/commit/dda04e5f1914248e15b1785c5ae57dc5e960b371))
* **jobs:** port website-contract counter, rank and scheduled-events ticks ([#33](https://github.com/TogetherWeOwn/two-bot-next/issues/33)) ([64ae194](https://github.com/TogetherWeOwn/two-bot-next/commit/64ae194088ecc5acb2a51f08553277fbd6b19b57))
* **leveling:** port transactional XP runtime and legacy replies ([#54](https://github.com/TogetherWeOwn/two-bot-next/issues/54)) ([0da949c](https://github.com/TogetherWeOwn/two-bot-next/commit/0da949c722cc9f9d185d834a7cbe9ce99097a598))
* **lfg:** port signup domain and transactional store ([#39](https://github.com/TogetherWeOwn/two-bot-next/issues/39)) ([6129e69](https://github.com/TogetherWeOwn/two-bot-next/commit/6129e696ad9bfdc04797da0683dfa60042063cd0))
* **moderation:** add channel moderation domain and durable store ([#22](https://github.com/TogetherWeOwn/two-bot-next/issues/22)) ([d32e342](https://github.com/TogetherWeOwn/two-bot-next/commit/d32e342ef81de6b733adf30b7c710cadf24e8812))
* **onboarding:** port picker, welcome and goodbye domain with funnel store ([#41](https://github.com/TogetherWeOwn/two-bot-next/issues/41)) ([db47381](https://github.com/TogetherWeOwn/two-bot-next/commit/db47381c90ddfd710ca3be69da7f4734a3291bbb))
* **prototype:** measured twilight gateway plus slash command ([#16](https://github.com/TogetherWeOwn/two-bot-next/issues/16)) ([09e5724](https://github.com/TogetherWeOwn/two-bot-next/commit/09e5724116eaf8a0c25026ef5b96f7c7c4b92d30))
* **raid:** port join burst and join-risk decisions ([#55](https://github.com/TogetherWeOwn/two-bot-next/issues/55)) ([e5e72f0](https://github.com/TogetherWeOwn/two-bot-next/commit/e5e72f057a85408d1eadc0524cf4c393ed72a9cf))
* **redirect:** port go.two.gg redirect server to Worker route ([#7](https://github.com/TogetherWeOwn/two-bot-next/issues/7)) ([5348fa1](https://github.com/TogetherWeOwn/two-bot-next/commit/5348fa1d6079bcb2d7277f869b130619cb8955b6))
* **router:** interaction router plus full command registry publish ([#57](https://github.com/TogetherWeOwn/two-bot-next/issues/57)) ([3c4fe98](https://github.com/TogetherWeOwn/two-bot-next/commit/3c4fe9805d3762a67d61d3cc2114dc2da835afb4))
* **rsvp:** port RSVP transitions, totals, and host check-in ([#24](https://github.com/TogetherWeOwn/two-bot-next/issues/24)) ([eb88087](https://github.com/TogetherWeOwn/two-bot-next/commit/eb880874e1a1fc92166a3d6945cc577bbf3e6ccb))
* **scaffold:** Cargo workspace and Container deploy skeleton ([#3](https://github.com/TogetherWeOwn/two-bot-next/issues/3)) ([2fe0042](https://github.com/TogetherWeOwn/two-bot-next/commit/2fe0042aea0f6297f58291cd4a85f4e816f6c278))
* **settings:** guild_settings hot reload with 15s version poll and audit ([#25](https://github.com/TogetherWeOwn/two-bot-next/issues/25)) ([430e438](https://github.com/TogetherWeOwn/two-bot-next/commit/430e438aad3202f6cdede177e4cc47f5075effb9))
* **sticky:** debounced re-post domain, store and migration ([#28](https://github.com/TogetherWeOwn/two-bot-next/issues/28)) ([c89e053](https://github.com/TogetherWeOwn/two-bot-next/commit/c89e05376aee62bfb8d784bba85f51071e1e0092))
* **voice:** add offline readiness and safe diagnostic contract ([#48](https://github.com/TogetherWeOwn/two-bot-next/issues/48)) ([43c7f61](https://github.com/TogetherWeOwn/two-bot-next/commit/43c7f6121774efcba4ffdea302c9dfc7e07933bf))
* **voice:** add pure ownership transition core ([#49](https://github.com/TogetherWeOwn/two-bot-next/issues/49)) ([a926ffb](https://github.com/TogetherWeOwn/two-bot-next/commit/a926ffb5ef7a9f6af68e999f47dea09beaba627a))
* **voice:** add pure vote-kick decision core ([#50](https://github.com/TogetherWeOwn/two-bot-next/issues/50)) ([e6694ee](https://github.com/TogetherWeOwn/two-bot-next/commit/e6694ee1c18012d686cd8cb4b377159302a3538b))
* **voice:** add standalone versioned configuration codec ([#51](https://github.com/TogetherWeOwn/two-bot-next/issues/51)) ([dcbe432](https://github.com/TogetherWeOwn/two-bot-next/commit/dcbe432e447df9965411d7835d28c9aa5aa8d62f))

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
- Feed command and polling plans, bounded RSS/YouTube/Twitch XML parsing, public-IP/redirect fetch policy, and guild-scoped, fenced delivery storage with crash-reconciliation outcomes. Runtime transport wiring remains default-off pending the shared router and REST executor.
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

### Fixed

* **build:** refresh stale Cargo.lock so --locked Docker build passes ([#19](https://github.com/TogetherWeOwn/two-bot-next/issues/19)) ([1aa9ce8](https://github.com/TogetherWeOwn/two-bot-next/commit/1aa9ce819d802fe8b9f387eb4cae329d8af5f9d2))
* **config:** isolate environment parser tests ([#52](https://github.com/TogetherWeOwn/two-bot-next/issues/52)) ([32ef2b6](https://github.com/TogetherWeOwn/two-bot-next/commit/32ef2b62226e9778ec879bc0d019b88bcfe1b4f7))
* **deploy:** wire explicit Container/DO bindings for staging and production ([#8](https://github.com/TogetherWeOwn/two-bot-next/issues/8)) ([66884ff](https://github.com/TogetherWeOwn/two-bot-next/commit/66884ff523bef839df94abb020e340c1fef7b751))
* **gateway:** park until checkpoint prerequisites are configured ([#59](https://github.com/TogetherWeOwn/two-bot-next/issues/59)) ([bd8d415](https://github.com/TogetherWeOwn/two-bot-next/commit/bd8d4155139c655e0edfec20520ce4ef85d850d2))
* **release:** preserve bootstrap Notes tail in first release ([#62](https://github.com/TogetherWeOwn/two-bot-next/issues/62)) ([9d1cc1d](https://github.com/TogetherWeOwn/two-bot-next/commit/9d1cc1d58f238bfb49698a5e51cfe0f6bac100bc))
* **worker:** forward environment on automatic container starts ([#21](https://github.com/TogetherWeOwn/two-bot-next/issues/21)) ([5436321](https://github.com/TogetherWeOwn/two-bot-next/commit/543632175675a642974ded84e18983da4c41d5e6))

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

### Notes

- Command/component wiring and the Discord REST reads stay on the S4 interaction router and REST executor slices; the outcome enums are the integration surface until they land. Scorecard and probe collection are not enabled by this change.
