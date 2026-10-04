# Channel moderation runtime integration

TOG-10174 wires `/purge`, `/slowmode`, `/lockdown` and `/unlock` into the shared
bot runtime. `TWO_MODERATION` remains default-off. This change does not activate
a live guild; verification uses synthetic interactions, loopback REST/gateway
doubles, and isolated test-service schemas, never production/staging credentials.

## Shared composition and dispatch

`bot::CommandRuntime` is the single startup composition owner. The channel
runtime uses the same `core::InteractionRouter`, cloned shared
`discord::ActionExecutor`, and SQLx pool as sticky/feed commands. Registration
uses `HandlerId::Moderation`; no second router, registry or HTTP client exists.

The shared router adjudicates the configured guild, feature gate and runtime
permissions. The invoking channel is authoritative. The channel domain validates
required reasons, purge 1–100 and slowmode 0–21600. Lockdown changes only the
@everyone SendMessages bit; unlock requires the recorded recovery seed and
restores exact original masks or deletes an originally absent overwrite.

READY publishes the router's complete gated registry, including the other
builtin slices. RESUMED-only startup resolves the application through the same
executor and synchronizes once per process. Failed publication remains retryable
on a later connection event. The current registry has no custom-command store;
when that slice lands, its reader must join the shared full-set publication.

Gateway dispatch admits tasks synchronously into three independently bounded
lanes: 16 message workers, 16 interaction workers and one registry worker. It
never waits for command SQL/REST or creates queued/spawned waiters. At saturation,
events are not admitted and cannot cause effects. Overlapping READY/RESUMED syncs
coalesce while publication is in progress. A scope guard aborts admitted work
when the shard exits or its supervisor cancels it. Interrupted channel work
retains durable uncertainty; cancellation never releases an ambiguous effect.

## Interaction lifecycle and durable exclusion

`ChannelModerationRuntime::respond` owns the channel interaction lifecycle:
ephemeral defer before SQL/REST with a two-second acknowledgement deadline, then
an original-response edit through the shared executor. Failed or ambiguous defer
admits no mutation. Replies suppress mentions and bound content. Edit failure
never repeats an accepted effect; logs/errors exclude interaction tokens.

Migration 0123 (main's `moderation_channel_executions`, shared with the website executor) holds the single fence. `claim_channel` reserves one
channel lane only for a current in-flight ticket, excluding different request
keys as well as duplicate retries. There is deliberately no expiry: ambiguous
HTTP outcomes or process loss require reconciliation, not another mutation.
The runtime DML role explicitly includes this table; the web-reader role does not.

`finish` atomically performs generation-fenced completion, audit insertion,
confirmed unlock recovery cleanup and lane release. False means no finalization
occurred and must never trigger another effect. SQL failure retains the claim,
lane and recovery seed; retry only persistence with the same ticket/result.
The older `complete` method alone does not release a runtime lane.

A failure releases the lane (via `finish` with a `refused` reply) only when it
proves no mutation was accepted: a local guard or build refusal, a confirmed
400/401/403/404/405 rejection, a 429 (Discord documents it as not processed),
or any failure of `/purge`'s history read. `/purge` runs the read and the delete
as separate phases, so a 5xx, timeout or unreadable body on the read cannot
strand the lane; the website executor already splits them the same way. A failed
delete, overwrite write or slowmode PATCH with an ambiguous result (5xx, timeout,
unexpected status) still keeps the claim and lane.

The first lockdown seed is persisted before PUT and repeated locks preserve it.
A proven rejected first lock can retire its new seed because the original state
was not changed; a rejected repeated lock keeps the original recovery seed.
Unlock without tracked recovery refuses instead of clearing a manual deny.

## Shared member ledger contract

The examined main does not contain the member runtime slice. Its branch uses
`timestamptz` for shared claim/audit timestamps. Additive migration 0124 converts
these three shared columns without changing applied 0120; channel SQL binds ISO
strings with `::text::timestamptz`. Lockdown timestamps remain TEXT. Legacy
row/result/generation preservation and both insert shapes have regressions.
This is a shared-schema contract, not proof of both actual runtime stores:
integrated compatibility must be checked when the member slice merges.

## Verification and remaining gate

PR CI is the Rust verification of record under the 2026-10-01 CEO decision on
TOG-11174; the private repository now uses `[self-hosted, two-selfhosted]` runners.
The deferred Operator cache rollout TOG-10906 does not gate PR CI. Controller
compilation still requires admitted bounded-wrapper leases; no direct/unadmitted
controller compilation or workflow dispatch is used. Local noncompiling checks
cover formatting, migration locks, Docker dependency fixtures, guarded channel
CI routing/job-isolation fixtures and the secret Debug guard.

Both required checks and nightly DB sweeps run in job containers with service-name
Postgres endpoints and no published host port. Fixed-loopback fixtures use a
job-private forward, never a host-network alias. Nightly routes all channel
store/runtime acceptance separately onto their guarded `agent_test` database;
no test is dropped or run against the broad sweep's incompatible bootstrap URL.

CI runs channel runtime/store acceptance and the shared command-runtime DB suite.
New integration regressions exercise complete-registry sticky/feed coexistence,
disabled-channel defer/audit/refusal, bounded admission/scope cancellation, and
a real Twilight shard that keeps checkpoints/heartbeats moving during a slow
channel mutation and retains the in-flight lane after cancellation.

Historical lifecycle checkpoint `25a032b5d91f3139e913bdb908100c79e7d0f30d` passed
15 runtime acceptance tests, 19 executor regressions and Discord DB-feature
Clippy before integration of newer main. Those results do not certify the new
composition/dispatch head. Exact-head PR CI and independent Code Reviewer
approval/squash merge remain required; no review, merge or deployment is claimed.
