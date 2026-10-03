# Containment port boundaries

TOG-9809 slice 5 ports **pure containment policy and quarantine planning only**.
Nothing in this slice registers an audit listener, reads environment variables,
connects to a database, posts an alert, or calls Discord. It does not arm anti-nuke.
Raid alerts and join-risk are subsequent slices. Temporary voice rooms and the
TOG-10075–TOG-10089 feature runtimes are outside this card's retained scope.

## Frozen source

All behavior is based on `TogetherWeOwn/two-bot` revision
`d5d1179348feb9157bcac8c875de9399d4f5c76a`, not a moving branch:

- [Classification and completion](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/moderation/containment.ts#L108-L172)
  — blob `f3447f72d42eb8fb2a36be6ad90e46de5456f511`.
- [Occurrence heat and incident claims](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/moderation/containmentStore.ts#L36-L150)
  — blob `283e877060ca4398c8f700a6752b00026c1e73b1`.
- [Role preflight, ordered removal and HTTP outcomes](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/moderation/containmentDiscord.ts#L4-L173)
  — blob `9d7c240194980d4718c1706b673e8847edf71d8a`.
- [Configuration defaults and protected bot](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/moderation/containmentConfig.ts#L44-L69)
  — blob `bd32af6aa6d1dacc8d5bde1cabd89391786bf936`.

## Decision contract

`DestructiveAction` derives its weight internally: channel/role deletion weighs
3; member kick/ban and webhook create/update/delete weigh 1. Unsupported names
are rejected. Audit IDs, guild/executor IDs, target IDs and occurrence timestamps
are provided by the eventual adapter, never guessed by policy.

`ContainmentPolicy::disposition` first classifies invalid or more-than-120s-old
entries as stale. Missing, trusted, protected and configured bot executors are
ignored. Otherwise an entry more than 5s ahead of processing time is ignored.
Both time boundaries are strict. Recorded ignored states are permanent evidence;
they must not be reclassified when the clock catches up.

`occurrence_heat` consumes already-claimed evidence, including the new trigger.
Heat is isolated by guild/executor and deduplicates supplied audit IDs. Only
`Observe` and `Contain` rows count. Like the legacy store, it queries the interval
`(trigger − window, min(trigger + window, processing time + 5s)]`, sorts by
occurrence time and audit ID, and finds the maximum sliding heat at endpoints at
or after the trigger. An exact-window separation is excluded. This supports
reverse delivery without summing two adjacent windows together. Defaults are a
60s window and heat threshold 5. Policy durations are explicit integer
milliseconds; environment parsing is not implemented in this slice.

`incident_candidate` proposes an incident with the triggering audit ID, state
`Containing`, and cooldown measured from **processing time**, not occurrence
time. An existing same-guild/executor incident blocks while its cooldown is
strictly later than now. `Uncertain` blocks indefinitely, including after
cooldown; other states, including stranded `Containing`, release at expiry.
No retry instruction is produced.

`plan_quarantine` returns dry-run, whole-plan refusal, or ordered role IDs. It
excludes safe roles, unheld roles and `@everyone`. If any held dangerous role is
managed or at/above the bot's highest role, **none** are removable. A managed-role
refusal takes precedence over hierarchy, as in legacy. Dangerous permissions are
KickMembers, BanMembers, Administrator, ManageChannels, ManageGuild,
ManageWebhooks, ManageRoles and ModerateMembers.

The eventual executor must remove roles sequentially, stop at the first failure,
and retain every confirmed removal. Removal HTTP 200/204/404 is success. Timeout,
429 and upstream unavailability are uncertain even with no confirmed removal;
any partial success followed by failure is also uncertain. Definitive rejection
with no confirmed removal is refused. A successful empty plan is contained.

## Required runtime fences (not implemented here)

A pure plan is **not** permission to execute it. A subsequent adapter must:

1. Keep legacy `TWO_ANTI_NUKE=1` enablement and default dry-run
   (`TWO_ANTI_NUKE_DRY_RUN` must explicitly equal `0` to arm).
2. Preserve [startup's staging and identity check](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/index.ts#L498-L511):
   staging guild `1545644954272137297`, application `1469137636663758888`, and
   verified identity before registration. Never use the production guild/tokens.
   Keep the staging-only fence until soak acceptance authorizes promotion.
3. Preserve [session-mode refusal of armed containment](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/index.ts#L179-L184).
   Dry-run remains allowed by this guard.
4. Atomically claim audit IDs and persist the exact disposition. Serialize
   guild/executor event claims and incident claims separately; recheck incident
   blockers inside the incident transaction. After starting an incident, persist
   the trigger as `Contain`. Core proposals alone provide **no** concurrency or
   restart guarantee. `containment_store` (migration
   `0370_containment_claims.sql`, legacy 0015 shape) now provides these claims
   but is not wired to the gateway, executor or any arming path.
5. Capture complete role/member snapshots, perform whole-plan preflight before
   writes, enforce the 5s request timeout, and never automatically retry unknown
   outcomes. Use the shared executor boundary rather than duplicating it.
6. Persist completion evidence, then deliver safe staff alerts with empty allowed
   mentions. Snapshot restore advice is advisory only, never automatic restore.

No production or staging databases may be used for tests or verification.
Future persistence tests use agent-testdb/scratch Postgres or CI service
containers; Discord-adjacent acceptance uses a mock Discord double only.

## Verification boundary

Unit tests exercise action weights, refusal precedence, strict time boundaries,
reverse delivery, guild/executor isolation, recorded future exclusion, cooldown,
uncertainty, safe-role preservation and whole-plan preflight. The in-process
`containment_acceptance` suite uses a scripted Discord double to test threshold
crossing, dry-run, preflight refusal, ordered partial failure and no automatic
retry. Its claimed-evidence fixture is **not** evidence of durable dedupe or
concurrent incident serialization. The ignored `containment_store` suite
(agent-testdb or the CI Postgres service) covers exactly-once audit-ID claims
under concurrency, serialized heat, one concurrent incident per guild/executor,
the in-transaction blocker recheck, dispositions across a reconnect and an
upgrade from the legacy 0015 schema. Gateway hooks, executor delivery,
startup gates and alert delivery are wired by `crates/bot/src/containment_runtime.rs`
(audit-entry observer seam, fenced construction, verified application identity,
dry-run-default plans, armed-only removals, shared-executor staff posts) with
mock-Discord plus ignored agent-testdb worker tests; staging soak acceptance
remains unimplemented and untested.
