# Voice component-id codec core

`two_bot_core::voice_custom_id` is a pure encoder/decoder for voice Discord
component ids (`two:voice:` prefix), following the codec pattern in
`crates/core/src/self_roles.rs` (`self_role_custom_id` /
`parse_self_role_custom_id`). It performs no I/O, holds no Discord, store or
clock types, and depends on no voice runtime state: the runtime binds buttons
and modals with the encoders and parses interactions back as untrusted input.

## Covered surfaces

- V3 private-join Approve / Deny / Block buttons
  (`join_custom_id(decision, room_id, request_id)`), bound to the room and the
  request ID. Request IDs are never reused within a room
  (`voice_private::PrivateRoom::next_request_id`), so a stale button can never
  answer a newer request; the runtime resolves the pair with
  `voice_private::PrivateRoom::decide`, which still checks the current owner.
- V4 vote-kick Yes / No buttons (`kick_custom_id(ballot, vote_id)`), bound to
  the vote ID. The ID must be the unique initiating interaction ID, never just
  the target member ID; the runtime casts the ballot with
  `voice_vote_kick::VoteKickCore::cast`.
- `/name` panel custom (`name_custom_custom_id`) and restore
  (`name_restore_custom_id`) buttons plus the custom-name modal submit
  (`name_modal_custom_id`), all bound to the room.

## Wire shape

| Action | Shape |
| --- | --- |
| Join approve | `two:voice:join-approve:<room_id>:<request_id>` |
| Join deny | `two:voice:join-deny:<room_id>:<request_id>` |
| Join block | `two:voice:join-block:<room_id>:<request_id>` |
| Kick yes | `two:voice:kick-yes:<vote_id>` |
| Kick no | `two:voice:kick-no:<vote_id>` |
| Name custom | `two:voice:name-custom:<room_id>` |
| Name restore | `two:voice:name-restore:<room_id>` |
| Name modal | `two:voice:name-modal:<room_id>` |

Ids are canonical nonzero decimal `u64` strings, never JSON numbers or padded
forms. Every encoded id is at most `MAX_VOICE_CUSTOM_ID_CHARS` (100),
Discord's component-id limit; even 20-digit snowflakes with `u64::MAX`
request ids fit.

## Parsing boundary

`parse_voice_custom_id` returns `None` for anything outside the voice
namespace: garbage (wrong prefix, unknown verbs, missing or extra segments),
overlong input (over 100 chars), non-numeric ids (signs, whitespace, hex,
floats, overflow) and zero ids, which no voice id ever holds. The
`two:voice:` prefix never collides with `two:lfg:` or `two:self-role:`, so
the router can dispatch on prefix without overlap.

`crates/core/tests/voice_custom_id.rs` pins golden shapes, rejection cases
and namespace separation, and proves round-trip plus the 100-char fit with
property tests over decisions, ballots and ids.
