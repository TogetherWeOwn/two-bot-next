# Moderation MAC marker acceptance

Card: TOG-12697. Parity gate: `two_bot_core::mac` in
`crates/core/src/mac.rs` (legacy `moderationAuditToken`,
`moderationAuditReason`, `parseModerationAuditReason`; see
`docs/audit-core.md` §`two_bot_core::mac`).

## What is pinned

The token is a stable 32-lowercase-hex one-way binding of
`(guild_id, idempotency_key)` — same inputs mint the same token, a
different guild or key mints a different one, and the inputs are not
recoverable from the token. The token alone proves nothing (threat model
in `mac.rs`); only the keyed MAC correlates.

Without a secret (`None` or empty) minting returns the reason unchanged
and parsing returns `None`, even for a well-formed marker. With the
public non-production secret from
`crates/core/tests/fixtures/moderation-mac.json` the minted marker
parses and verifies, including the exact Node wire vector; tampering
with any one authenticated field (action, actor, token, mac) while
keeping the public shape fails verification, as do a wrong secret and a
wrong guild. Untouched trailing prose stays correlated.

Over-long reasons truncate only the human suffix to the 512 UTF-16
budget: the exact boundary passes through unchanged, one unit over
shortens to exactly the budget ending in `…`, and long ASCII /
multi-byte / astral suffixes all fit, keep a whole-scalar prefix, and
leave the marker verifiable. Short reasons are untouched.

`is_auditable_action` covers the nine service verbs in
`ModerationAction::ALL` publish order plus
`moderation.unban_scheduled`, and refuses unknown verbs, wrong case,
and surrounding whitespace.

## Key pins

- All assertions go through the existing public `mac` API only; no new
  source, no operational key. Secrets come only from the public
  non-production fixture vectors.
- The tamper cases keep valid hex/snowflake/action shapes so each one
  exercises MAC verification rather than shape rejection.

## Acceptance

`crates/core/tests/moderation_mac_acceptance.rs` pins each row above.
The unit/property rows stay pinned inline in `mac.rs` and are
deliberately not repeated here.

Run:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test moderation_mac_acceptance
```
