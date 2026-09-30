# Operational audit core

This is the framework-free part of TOG-9810, not a deployed audit sink.

## Implemented

- `two_bot_core::audit`: seven operational row kinds, mirror formatting and
  identity markers, audit/voice/moderation channel fallback, guild fence,
  mirror-source feedback-loop suppression, deterministic delivery nonce, and
  the legacy kill-switch decision/transition model.
- `two_bot_core::classify`: member role/nickname-change metadata, voice
  join/leave/move boundaries, raw message edit/delete dispatches, and fourteen
  Discord audit-log action classifications. Voice classification does not
  create or manage temporary voice rooms.
- `two_bot_core::mac`: legacy moderation token and HMAC reason markers,
  constant-time verification of validated fixed-length signatures, and explicit
  credential-file/env loading. An unreadable configured credential fails;
  it never falls back to another credential.

Classifier output contains IDs, role lists, counts and flags, never message
bodies, nicknames, usernames or the free-form moderation reason. `AuditEvent`
is a plain data structure: callers must not put those sensitive values into
its generic metadata. Mirror adapters must disable allowed mentions.

Correlation requires **both** a valid guild-bound MAC and an executor matching
this bot's user ID. Without a configured secret, reasons remain unchanged and
entries use `discord-audit:` identities. A valid token by itself proves nothing.
The MAC is the legacy truncated 64-bit HMAC; this port does not redesign that
wire contract. The independent Node crypto vector is asserted in a Rust test.

## Source contract

Verified against TogetherWeOwn/two-bot at
`b0a26a5e3882dd0784d208079f309893e2ede7e8`:

- [`src/audit/events.ts`](https://github.com/TogetherWeOwn/two-bot/blob/b0a26a5e3882dd0784d208079f309893e2ede7e8/src/audit/events.ts)
- [`src/audit/discordEvents.ts`](https://github.com/TogetherWeOwn/two-bot/blob/b0a26a5e3882dd0784d208079f309893e2ede7e8/src/audit/discordEvents.ts)
- [`src/audit/moderationIdentity.ts`](https://github.com/TogetherWeOwn/two-bot/blob/b0a26a5e3882dd0784d208079f309893e2ede7e8/src/audit/moderationIdentity.ts)
- [`src/audit/service.ts`](https://github.com/TogetherWeOwn/two-bot/blob/b0a26a5e3882dd0784d208079f309893e2ede7e8/src/audit/service.ts)

Legacy `rota_notice` is intentionally excluded: `record()` refuses it before
store/claim/delivery. The workspace's `serde_json` preserve_order feature keeps
mirror metadata in insertion order. Timestamp parsing accepts Discord's RFC-3339
instants with an explicit zone, not JS Date.parse's loose date strings. Malformed non-scalar
counts are not interpreted as numbers. Unicode truncation respects the legacy
UTF-16 budget but does not split Unicode scalars.

The kill-switch model preserves legacy fail-open on a failed switch read.
That is **not** permission to send after a failed durable write: the runtime
must return before delivery if recording fails.

## Acceptance still open

TOG-9810 is not complete until the following have been implemented and tested:

1. Postgres audit rows and idempotent insertion, durable pending delivery,
   lease/claim ownership, acknowledgement recovery and quarantine.
2. Mock-Discord mirror adapter with guild/privacy/permission gates,
   `allowed_mentions: { parse: [] }`, deterministic enforced nonce, history
   reconciliation and no blind resend after an ambiguous accepted post.
3. Persistent delivery kill switch checked per row and immediately before
   each send, with held claims safely released.
4. Gateway classification and successful-moderation recording wired into the
   runtime; readyz/retry lifecycle exercised in a container.

Database tests may use only agent-testdb or CI Postgres service containers;
Discord tests use doubles, never the production guild or token. No database or
Discord endpoint is accessed by these core unit tests.

Jobs and gateway session persistence moved to TOG-10090, TOG-10092 and
TOG-10094. Temporary voice-room features moved to TOG-10091 and its V-slices.
None are included here.

## Verification

```sh
cargo test -p two-bot-core --lib --locked -j 1
cargo clippy -p two-bot-core --all-targets --locked -j 1 -- -D warnings
cargo fmt --all -- --check
git diff --check
```

The workspace-wide integration suite is delegated to CI; this slice only needs
an incremental core build locally.
