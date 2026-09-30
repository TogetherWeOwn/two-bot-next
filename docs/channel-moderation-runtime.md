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
recovery-generation rollback. Router/mock REST acceptance additionally proves
permission/feature/guild gates, required reasons and numeric bounds, purge affected
counts, stored-result replay, first seed persistence before PUT acceptance, repeated
lock preservation, exact restore/delete, and untracked unlock refusal. 5xx, 429 and
five-second timeout cases retain the lane and recovery state across same/different
keys. Injected audit failures retry only finalization or retain the lane if exhausted.

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

The feature-gated `discord::ChannelModerationRuntime` and shared-router registration
are implemented; they never create a private router or HTTP client. The runtime
finishes accepted effects atomically and retries that persistence step only. A proven
rejected first lock retires its newly recorded seed because the original overwrite
was never changed; a rejected repeated lock keeps the original recovery seed.

`ChannelModerationRuntime::respond` now defers an ephemeral response before any
SQL or channel HTTP, with a two-second acknowledgement deadline. A rejected or
ambiguous defer admits no mutation. It edits the original response through the
shared executor with mention suppression. Persistence failures produce a bounded
reconciliation reply; edit failures never repeat an accepted effect. Five additional
mock/isolated-DB regressions prove this lifecycle, bringing runtime acceptance to 15.
The shared edit seam also validates IDs/content before sending.

Remaining: startup composition, complete-set publication on READY and bounded async
gateway dispatch without blocking heartbeat/checkpoint work. Automation catalogue
loading is not yet integrated; never replace enabled custom commands with an empty
partial publish set. Exercise both actual stores once the member slice merges.
Then ship one PR, exact-head CI and independent Code Reviewer squash merge.
No startup activation, PR review or merge is claimed yet.

## Build admission checkpoint — 2026-09-30

Lifecycle commit `25a032b5d91f3139e913bdb908100c79e7d0f30d` passed 15 runtime
acceptance tests, 19 executor regressions and Discord all-target DB-feature Clippy.
These results precede this branch's integration of `origin/main` at `44338b2`,
including release 0.2.0 and mandatory bounded Cargo admission (PR #56).
Released changelog notes are preserved; this slice remains Unreleased.

Verification through the required wrapper was refused with exit 75:
`not a real directory: /paperclip/.cache/two-bot-next-bounded`.
Do not substitute the old shared target, create the host pool, or fabricate quota
receipts. The existing [Operator rollout](https://github.com/TogetherWeOwn/two-bot-next/pull/56)
tracked by TOG-10906 owns provisioning and evidence. Current integrated-head Rust
verification is blocked until that rollout completes; no post-integration green
result is claimed. Resume with bounded-wrapper verification, then the remaining
startup/READY/dispatch work and exact-head independent review.
