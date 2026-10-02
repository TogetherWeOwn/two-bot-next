# Join-request lifecycle acceptance

Acceptance fixture for the private-room join-request flow: raise → pending →
owner decision → withdraw. It pins `two_bot_core::voice_private::PrivateRoom`
behaviour only, derived from [the voice-room specification](voice-rooms.md#v3-owner-room-controls)
and [the private-room core seam](voice-private-core.md). It requires no `db`
feature, Discord wire types, clock, store, or external I/O.

## Lifecycle under test

- **Raise:** an outsider entering the live Join channel raises exactly one
  pending `Approve / Deny / Block` request for the current owner
  (`enter_join_channel` → `EntryOutcome::Raised` + `AskOwner`). Re-entering
  while pending deduplicates (`AlreadyPending`) with no new effects.
- **Decide:** only the current owner answers (`decide`). Approve grants Connect
  and moves the member in (`GrantConnect` + `MoveMember`); Deny grants nothing
  and the member may request again with a fresh request ID. Non-owner answers
  are refused (`NotOwner`); answering a non-pending request is refused
  (`RequestNotPending`).
- **Withdraw:** owner transfer (`set_owner`) and room deletion (`delete_room`)
  withdraw all pending requests (`WithdrawRequest`) without granting access.
  Request IDs are never reused, so buttons bound to a withdrawn request can
  never match a newer one.
- **No bypass:** repeat Join-channel entries while pending stay pending and
  grant nothing. Only `decide` resolves the pending state; an approved member
  then enters with access and raises no further request.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_join_request_lifecycle
```

The fixture covers raise-then-dedupe, owner accept/deny effects, non-owner
refusal, non-pending refusal, transfer and delete withdrawal, and the
no-bypass entry path. No tests in this fixture use a database, Redis,
Discord, or a staging identity.

## Residual parent work

Request-button routing and expiry, ephemeral replies, per-room serialization
and persistence, and runtime wiring remain outside this slice (see
[voice-private-core.md](voice-private-core.md)). Unit tests establish domain
behaviour only, not runtime parity or staging readiness.
