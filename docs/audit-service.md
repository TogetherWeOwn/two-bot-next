# Operational audit mirror service

[TOG-10345](/TOG/issues/TOG-10345) adds `two_bot_core::audit_service`
(`db` feature) and `two_bot_discord::audit_mirror`. Together they are the
delivery engine for [TOG-9810](/TOG/issues/TOG-9810): events are recorded
before anything is sent, every send is fenced by the persistent kill switch,
and an ambiguous post is reconciled against mirror history instead of
resent. **No runtime wiring is included** — gateway classification and
moderation recording are TOG-10346. Nothing here touches a production or
staging guild, token or database.

## Pieces

- `two_bot_core::audit_mirror` (no `db` needed): the `AuditMirror` contract
  plus pure helpers — `mirror_channel_policy` (privacy/guild gate),
  `find_mirror_in_page` (author + identity-marker + boundary match),
  `next_snowflake` / `snowflake_at_or_after` / `page_floor_id` (scan
  arithmetic), `DEDUP_PAGE_LIMIT` and `HISTORY_PAGE_LIMIT`.
- `two_bot_core::audit_service`: `AuditMirrorService<M: AuditMirror>` with
  `record`, `deliver_entry`, `deliver`, `drain_pending` (≤25 rows, matching
  `pending_ids`), `transitions`, and the `with_pre_send_gate` seam used by
  the fault-injection suite to land a halt inside the prepare→POST window.
- `two_bot_discord::audit_mirror`: `impl AuditMirror for ActionExecutor`.
  The executor owns pacing, the nonce/`allowed_mentions` wire shape and the
  REST surface; the adapter maps `DiscordError` to `MirrorError` and parses
  channel documents/history strictly. There is no second HTTP client.

## Mirror classification

`MirrorError::Rejected` means provably unsent or refused (4xx, malformed
payload): a post retry is permitted under the store's bounded policy, a
preflight read quarantines. `RateLimited` (429) defers; `Uncertain`
(timeout, 5xx, connection failure) means the request may have landed —
the durable boundary is kept and only reconciliation may close the row.

`channel_document` reduces `GET /channels/{c}` to `guild_id` plus the
`@everyone` overwrite (type 0 whose id equals the guild id). A missing
`guild_id` yields `""` and is fenced `WrongGuild`; a missing overwrite
array yields `everyone: None` and is fenced `PublicChannel`; a malformed
row or a non-object body is `Rejected` — never silently "absent".
`channel_history` reduces `GET /channels/{c}/messages` to
`{id, author.id, content}` rows, skipping malformed entries (reconciliation
needs only one readable marked row) while a non-array page is `Rejected`.

## Send protocol

`deliver` dispatches on `claim.intent()`. The send path, in order:

1. Missing destination → `Unclaimed` (store-only rows never deliver).
2. `check_halt` reads the durable switch fresh: halted →
   `release_unattempted` → `Held`. The row stays pending with zero attempts
   and cannot be reclaimed while halted.
3. Preflight `channel_document` + `mirror_channel_policy`: `Rejected` →
   quarantine `PermissionRevoked`; `WrongGuild` → `EvidenceConflict`;
   `PublicChannel` → `PermissionRevoked`; `RateLimited`/`Uncertain` →
   `defer_preflight` → `Deferred` (no POST attempt counted).
4. Bounded dedup scan (≤ `DEDUP_PAGE_LIMIT` pages): page 0's newest id
   seeds the `search_before` boundary via `next_snowflake`; an empty
   channel seeds `"0"`. A bot-authored row carrying this event's exact
   `audit-event:{entry};` marker at/above the boundary is adopted
   (`note_accepted` + `complete` → `Reconciled`) — no second POST.
5. `prepare_send(claim, boundary)`: `Prepared`, `Halted` (→ release →
   `Held`), or `LostClaim` (→ `Unclaimed`).
6. The `pre_send_gate` seam, then a **second** `check_halt` immediately
   before POST: a halt landed in the window records a definite
   non-acceptance (`fail_attempt(DefinitelyRejected)` → `Held`), keeping
   the row pending and never dropping it.
7. `post_mirror` with the row's stored nonce (`delivery_nonce` fallback)
   and `format_audit_event` content. `Ok(id)` → `note_accepted` +
   `complete` → `Delivered`. `Ok("")` or `Err(Uncertain)` →
   `UncertainAcceptance` → `Ambiguous` (boundary retained). `Rejected` or
   `RateLimited` → `DefinitelyRejected` → `Rejected` (bounded retry).

## Reconcile protocol

A `Reconcile` intent means acceptance is uncertain. The recorded
`mirror_message_id` completes directly with zero Discord reads. Otherwise
history is scanned from `search_before` downward: a marked bot-authored
row at/above the boundary is adopted; an ambiguous read keeps the row
fenced (`Ambiguous`, boundary retained); a rejected read quarantines
`PermissionRevoked`; scanning past the boundary or hitting an
exhausted/empty history quarantines `MarkerMissing`. The bot never resends
on reconciliation alone.

## Kill switch

The service reads `delivery_halt` fresh on every check and feeds
`KillSwitchSnapshot`: a read failure fails open (legacy parity) but is
logged as `KillSwitchLog::ReadFailed` in `transitions()`; engage/disengage
edges are logged once each. A failed durable *record* still returns `Err`
before any delivery — fail-open never permits send-after-failed-write.

## Testing

- `crates/discord/tests/audit_mirror.rs` (8 tests, `MockRest`): wire shape
  (string nonce, `enforce_nonce`, `{parse: []}`), status classification,
  timeout→`Uncertain`, document/history parsing and path/query shape.
- `crates/core/tests/audit_service.rs` (14 tests, `#[ignore]` agent-testdb
  lane + scripted in-memory mirror): nonce enforcement, routing outcomes,
  accepted-post ack loss → reconcile without resend, restart with recorded
  acceptance, definite rejection retry, boundary-bounded history scan,
  marker-missing quarantine, ambiguous-read fencing, privacy/guild/404
  refusals, preflight deferral, stale-claim fencing, halt at claim and in
  the prepare→POST window with safe release, and the bounded drain batch.

```sh
cargo test -p two-bot-core --features db --test audit_service --locked -- --ignored
cargo test -p two-bot-discord --test audit_mirror --locked
```
