# S4 onboarding domain and store

This is the domain/store slice of TOG-10086, not a deployed onboarding handler.
The shared interaction router (TOG-10075) and REST executor (TOG-10076) must be
merged before runtime integration. No private dispatcher, Discord HTTP client,
production/staging database probe or live Discord action is included here.

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

## Verification

```sh
cargo test -p two-bot-core --locked
cargo clippy -p two-bot-core --features db --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo test -p two-bot-core --features db --test onboarding_store --locked -- --ignored
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
