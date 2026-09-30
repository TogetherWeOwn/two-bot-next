# Channel moderation runtime integration

Work in progress for TOG-10174. This document records integration constraints,
not activation or completion. TWO_MODERATION remains default-off; no live Discord
or production/staging database is used for verification.

## Shared seams

Verified after integrating `origin/main` at `e12aee0` (router #57, executor #63,
channel domain/store #22):

- `core::InteractionRouter` already routes and publishes all four channel verbs
  through `HandlerId::Moderation`. `InteractionHandler` is an identity registration
  seam only; it does not execute handlers. Do not add a second router/registry.
- `discord::route_interaction` supplies the routed slash outcome from Twilight.
  Options use the interaction's own channel, not a caller-specified channel.
- `discord::ActionExecutor::execute_channel` handles purge, slowmode, explicit
  overwrite PUT and overwrite DELETE. `get_everyone_overwrite` supplies a strict
  validated read. These channel calls are single-attempt with a five-second abort.
- Use `core::plan_lockdown`/`plan_unlock`, not executor `execute_outcome`'s generic
  unlock fallback. Unlock must refuse without a stored recovery record.
- Startup currently has no shared router/executor construction or asynchronous
  interaction dispatch. Wire it alongside the existing funnel pipeline without
  waiting for moderation HTTP/SQL in the heartbeat/checkpoint loop. READY must
  publish the complete shared command set, not only this slice.

## Durable channel lane (migration 0123)

`ChannelModerationStore::claim_channel` grants one channel lane only to a current
in-flight ticket. Different request keys cannot mutate the same channel while the
lane is retained. There is deliberately no expiry: after an ambiguous outcome or
process loss, neither a same-key retry nor a new key may guess the remote result.
Reconciliation is required before safely releasing retained state.

`finish` uses one transaction for generation-fenced claim completion, audit,
confirmed unlock recovery cleanup, and lane release. False means no finalization
occurred; it must never trigger another Discord effect. SQL failure retains the
claim/lane and recovery state; retry only finalization with the same ticket and
observed result. Safe `release` cascades to this claim's lane. The older `complete`
method alone does not release a runtime lane: runtime handlers must use `finish`.

Isolated test-service coverage proves distinct-key exclusion, safe release,
immutable completion, audit failure rollback, stale-ticket/action rejection and
recovery-generation rollback. End-to-end router/mock REST coverage is still pending.

## Shared member ledger compatibility — schema contract aligned

The member slice is not yet in the examined main. Read-only comparison against
`origin/TOG-10078-two-bot-next-s4-member-moderation-handlers-ban-tempban-kick-timeout-warn-unban-sweep`
shows that its `0110_moderation_member.sql` uses timestamptz for shared audit and
claim timestamps, and `0112_moderation_legacy_timestamps.sql` converts existing
TEXT columns. Additive channel migration 0124 now aligns those same three shared
columns, leaving applied 0120 unchanged. Channel SQL binds ISO strings with
`::text::timestamptz`, matching the member store. Lockdown timestamps remain TEXT.

Isolated regressions prove legacy-row/result/generation preservation, repeat-safe
conversion, and member-shaped/channel-shaped inserts into one ledger. This proves
the shared schema contract, not integrated member runtime execution: a both-store
check remains necessary once the actual member slice merges.

The shared non-timestamp columns and primary keys match. Member inserts omit
`claim_token`, so the additive 0121 default is structurally compatible; member
key-only APIs must not replace channel ticket-fenced completion/release.

## Next implementation step

Add the channel runtime adapter, registering four handlers on the shared router,
extracting and validating reason/count/seconds, and using the shared executor.
Claim before effects, take the durable lane, preserve the first seed before PUT,
restore exactly before `finish`, and retain both claim and lane on ambiguity.
Add gate/refusal/success/affected-count/replay mock REST tests and shared-schema
compatibility regressions; then wire startup/READY and asynchronous gateway dispatch.
Ship one PR, exact-head CI, and independent Code Reviewer squash merge.
