# S4 onboarding domain and store

This records the domain/store slice of TOG-10086 and its runtime follow-up,
TOG-10278, not a deployed onboarding handler. The shared interaction router
(TOG-10075) and REST executor (TOG-10076) are merged prerequisites. No private
dispatcher, Discord HTTP client, production/staging database probe or live
Discord action is included here.

## Parity source and intentional differences

`docs/parity.md` freezes TogetherWeOwn/two-bot at `d5d11793`. Sources:

- `src/onboarding/{flow,catalog,session,mode,anchorEvent}.ts`
- `src/discord/{onboarding,sessionWelcome,anchorWelcome}.ts`
- `src/core/events.ts` (funnel vocabulary and idempotency keys)

The new switch accepts `legacy`, `session`, or `anchor` through
`TWO_ONBOARDING_MODE`; legacy selected anchor separately by channel-config
presence. This is the card-directed mode change. Unset/empty remains legacy.

Dry run is deliberately per-mode, not a universal mute: game role writes,
legacy/anchor welcomes and session goodbyes are suppressed; roleless session
welcomes and session picker acknowledgements/routing records remain active.
This matches frozen `sessionWelcome.ts` (dry run logs but does not return before
its welcome send). Session mode also suppresses leveling reward-role writes.

Session submissions with unknown keys route nowhere and offer a retry, matching
the frozen spec. Legacy main after the freeze changed partially-stale session
handling; this port does not silently change the frozen baseline.

The anchor welcome computes the next/live Sunday 20:00 America/New_York
occurrence from the provided clock, including US DST boundaries. The date
arithmetic uses 2007+ US DST rules; it is not a general IANA timezone engine.
Scheduled-event creation/mirroring and temporary voice features are not part of
this slice.

## Runtime integration contract

1. Register only the selected mode's welcome handler. Pass ungated joins and
   **only** `pending: true -> false` member updates through `welcome_trigger`
   and `decide_prompt`; ignore bots and still-pending members. Session handlers
   must check the configured guild. S3 continues owning join/leave/gate funnel
   facts; do not duplicate those writes.
2. Resolve the first postable guild landing/goodbye channel from current
   settings. Never accept a DM target. Anchor uses its configured postable
   voice-text channel. The core names the outcome; live permission and guild
   checks remain the adapter's responsibility.
3. If the welcome outcome is a post, acquire `onboarding_store::begin_prompt`
   **before sending**. `None` means skip. Hold the returned `PromptGuard` over
   the executor's bounded send; call `record_sent` only after Discord accepts
   the post. A failed send drops the guard without writing a marker, allowing
   retry. The SQL member lock serializes concurrent handlers/processes.
   `has_onboarding_prompt` is only a read fast path, not a send claim.
4. Welcome allowed mentions permit exactly the new member, with empty role and
   general parsing lists. Attach the games/session menu according to
   `PickerKind`; anchor attaches **nothing**, not even a footer. Build games
   with min 0/max 10 and current game-role defaults; session min 1/max 2.
5. Dispatch `two:onboarding:games` only outside session mode, and
   `two:onboarding:session` in session mode. Defer ephemerally. Execute game
   add/remove-to-match effects; on role failure use `PICKER_ROLE_FAILURE_REPLY`
   and write no successful selection/routing rows. On success, re-run
   `plan_game_selection` with **post-grant** visibility, then build the
   final linked reply with `game_picker_reply` before recording routing. `GamePickerOutcome.reply/routed`
   are provisional at the pre-write point, not an authoritative post-grant
   permission answer. Clearing game roles records no selection/routing row.
6. Session selection has no role effect. A routed plan records the legacy
   `channel_routed` row with `session-picker` source; no
   `game_roles_selected`. Re-selection is repeatable at a new timestamp.
7. On session member removal, render `GoodbyeEffect` with
   `MentionPolicy::None`: `allowed_mentions` must have empty parse/users/roles
   lists even when the supplied username contains mention syntax. No DM or
   leaver ping. The S3 leave funnel row is separate.
8. Anchor posts record `onboarding_prompted`, then `channel_routed` with only
   the anchor channel and degraded=0 after successful delivery, like legacy.

The prompt guard prevents normal concurrent/redelivered successful sends; it is
**not exactly-once across a process crash between Discord acceptance and the
Postgres commit**. That cross-system ambiguity is inherited from legacy's
send-then-record ordering. Do not record a failed send as success to hide it.

## Runtime wiring checkpoint

The shared S4 router and executor are merged. The runtime follow-up uses
`route_interaction` and its existing `ComponentHandler::GamePicker` /
`SessionPicker` outcomes; the router's `InteractionHandler` trait identifies
slash owners, not async component callbacks. No separate custom-ID dispatcher
will be added.

`onboarding_config` reads only deployment routing fields, rejects malformed
snowflakes, and never falls back to a production guild or channel. Session
requires its per-guild destinations; anchor requires its configured channel.
The selected mode remains explicit (the domain/store slice's intentional
three-mode switch), not inferred from the anchor-channel setting.

`onboarding_messages` builds Twilight 0.17.1 menus and explicit mention policies.
The shared `ActionExecutor` now supports component-bearing channel posts,
single-role add/remove, and original deferred-response edits. These are bounded,
single-attempt mutations, not a private client; empty components are omitted on
posts so the anchor welcome attaches nothing. Role mutations use the shared
110 ms pacing lane and preserve unrelated roles. Component posts use the shared
mass-mention neutralization, scalar-safe text bounds and effective-create guard.
They accept only the domain's typed `MentionPolicy`: no mentions by default, or
exactly one welcome member, never arbitrary parse/role/multi-user/reply policies.
Goodbye usernames remain captured verbatim for recovery, but outbound text is
sanitized. Original-response edits retain the shared truncation/clearing contract
and fully suppressed mentions; they never gain the welcome exception.

The gateway now captures member state before S3 updates/removes it: ungated
joins, cached pending true-to-false transitions, and session goodbye joined-at.
A missing cached member does not establish a gate transition. S3 remains the
only writer of membership funnel facts. Asynchronous, bounded feature workers
call the shared component router and executor without blocking shard polling.

Each relevant event reads a consistent settings snapshot. Hot stored overrides
win; deletion restores deployment defaults. Mode and guild identity remain
immutable deployment fields. Effective channel access is resolved from fresh
guild/member/role reads and channel overwrites, in Discord overwrite order.
Foreign guilds, DMs, malformed evidence and unpostable destinations fail closed.
Game role changes are serialized per member and routing reads fresh roles after
all changes; unavailable fallback hubs are never linked. View-only game routing
accepts the hub's forum type, but a plain welcome cannot post to a forum. Existing
game submissions are gated by mode, not by destinations for posting new menus.
Permission reads preserve unavailable REST evidence separately from proven
403/404 absence or permission denial. Individually successful role/member reads
can disagree, and unreadable channel schemas are not proven denials: an unsent
welcome/goodbye retains its payload for bounded recovery on either case. Session
routing reads only deduplicated submitted known destinations; an unselected room
cannot add an availability dependency. Every selected room still requires live
permission evidence, and unknown keys still poison the whole routing plan.

Combined `onboarding_tests` exercise these adapters through actual mock HTTP
requests and isolated agent-testdb schemas, including two independent pools for
concurrent PromptGuard delivery. They prove mode/dry-run welcomes, accepted-send
markers, rejected-send retry, role matching, post-grant routing, roleless session
routing, hot settings, anchor attachments, empty goodbye mentions, pre-update
capture, and single-owned S3 facts. They do not prove deployment or crash-safe
gateway feature delivery.

## Delivery recovery checkpoint

Migration `0361_gateway_onboarding_jobs.sql` stores captured welcome/goodbye
state and component delivery receipts in the **same transaction** as the S3
funnel batch and sequence. Workers claim committed rows, not an in-memory copy
of a member cache. The original dispatch clock remains stable on retry. Session
reset/expiry clears only the gateway session, never these delivery rows.

A single shard owner may have at most 32 unfinished durable rows, but admits at
most two onboarding workers before claim/settings acquisition. Production opens
**distinct** pools within the existing five-connection gateway subsystem budget:
one gateway-only connection for checkpoints, funnel batches and queue operations,
and four feature connections shared by onboarding and the sticky/feed command
runtime. Pool clones do
not provide isolation. Feature transactions and member locks still span REST;
they cannot consume the gateway reservation. The separately supervised HTTP/jobs
pool retains its existing budget; this partition does not increase it.

Queue capacity failure rolls back the dispatch, including its checkpoint.
Worker errors/timeouts stop the essential runner, leaving the captured job for Container restart.
Restart reclaims interrupted workers, at most three attempts per job; exhaustion
fails closed until an authorized correction addresses the cause and resets that
specific failed job. Never substitute credentials or repeatedly restart to
conceal a permissions/configuration failure. Completion clears the payload and
keeps a receipt; `completed` means worker handling ended, not necessarily that a
message was sent (dry-run/duplicate/ignored/error-replied commands are terminal).

Interaction callbacks are deliberately **not replayed after process restart**.
No callback token, full interaction payload or bot token is written to the queue.
An ID-only receipt is marked `interrupted` when the process-local callback is
unavailable. The member must open the menu and submit a fresh selection; a
component's three-second initial acknowledgement deadline and potentially
partial role writes cannot safely be reconstructed from its gateway sequence.
Interrupted receipt counts are operational evidence, not successful responses.
Within a live process, uncertain callbacks attempt an error edit without role
replay; post-defer errors/timeouts get one bounded honest error edit. A delivered
error reply is terminal and writes no successful selection/routing rows.

Anchor `onboarding_prompted` and `channel_routed` now commit together under the
PromptGuard. Game selection/routing and session routing success rows are staged
inside their processing transaction, which rolls back when the final response
fails. Discord acceptance versus database commit remains a cross-system
ambiguity: neither the queue nor these transactions claims exactly-once sends
across a crash in that window. Session goodbyes are at-least-once across such a
crash; the legacy welcome marker suppresses retries after its successful commit.

Ingress now polls Twilight independently of the ordered SQL/pipeline owner through
an explicit 32-packet buffer. Overflow fails the essential runner; no dispatch is
silently discarded. Eligible shared-router components begin an ephemeral callback
through the shared executor with a 2.5-second receive-relative deadline, without
settings SQL or feature-worker admission. The ACK JoinSet is owned by the ingress
future; shutdown/error cancels it together with feature workers. Memory-only tickets
allow selection processing only after ordered checkpoint/job COMMIT and confirmed
ACK. Restart still retains only the token-free interruption receipt.

Ingress snapshots each dispatch's session, sequence, occurrence time and transport
generation before later packets can change Twilight state. Sequence replays are
fenced before ACK and again by the store. Transport invalidation immediately removes
readiness; an older pending SQL completion cannot restore Connected. Queued stale
interaction tickets are marked interrupted rather than applied. Settings/config
failures and disabled-mode submissions after defer receive bounded honest edits,
with no successful selection/routing rows or role writes.

**Not review-ready:** ingress/ACK repair head `8ff9bf5` passed required check
[36813095675](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36813095675).
All four wire-clock/blocked-SQL, settings-timeout, overflow and unconfirmed-ACK
ingress regressions passed, along with saturation/admission regressions, the
25-test gateway suite, 11 sticky runtime acceptances and one real binary lifecycle
acceptance. The optional JSON lifecycle assertion was skipped because the binary
does not support `LOG_FORMAT=json`. This is evidence for that preceding head,
not for the reconciliation with main `8af9b23` (shared command runtime and outbound
message safety). The complete reconciled head still requires exact-head green CI
and independent review on the same review card. Database isolation alone does not
establish Discord's three-second ACK budget.
Local Rust compilation has no certified bounded admission in this workspace; use the existing authorized CI
service containers, never a speculative local build or target-directory bypass.
Do not enable this checkpoint as a live onboarding flow or claim deployment
parity.

Framework references:

- https://docs.rs/twilight-http/0.17.1/twilight_http/request/channel/message/struct.CreateMessage.html
- https://docs.rs/twilight-http/0.17.1/twilight_http/client/struct.InteractionClient.html#method.update_response
- https://docs.rs/twilight-model/0.17.1/twilight_model/channel/message/component/struct.SelectMenu.html
- https://docs.discord.com/developers/topics/permissions (live permission resolver contract)

## Verification

```sh
cargo test -p two-bot-core --locked
cargo clippy -p two-bot-core --features db --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo test -p two-bot-core --features db --test onboarding_store --locked -- --ignored
cargo test -p two-bot-discord --test onboarding_wire --locked
cargo test -p two-bot --locked onboarding_ -- --include-ignored
```

The DB proof is opt-in and hardwired to
`postgres://agent_test@agent-testdb:5432/postgres` with an empty password. It
never reads `DATABASE_URL` or Discord credentials. Each run creates two unique
schemas and installs `search_path` on **every** pooled connection. It verifies
both schemas' migration constraints/indexes, replay-safe DDL, legacy row keys,
repeatable metadata, 12 concurrent mock deliveries per mode through two pools,
failed-send retry and per-mode dry run, then drops only its own schemas and
asserts zero remain. The mock records plain-data effects; it does not prove
shared-router/Twilight HTTP integration, which belongs to the follow-up.
