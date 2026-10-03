# Router voice-prefix seam

`two_bot_core::voice_custom_id` mints voice component ids under the
`two:voice:` prefix (V3 private-join Approve / Deny / Block buttons, V4
vote-kick Yes / No buttons, the `/name` panel buttons plus the custom-name
modal submit). `InteractionRouter` (`crates/core/src/router.rs`) dispatches
components and modal submits by exact id first, then the `two:lfg:` and
`two:self-role:` prefixes. It has **no voice branch yet**: every `two:voice:`
shape falls through to `ComponentOutcome::Unknown` (inside the configured
guild) or `Ignore` (outside it), on both `route_component` and `route_modal`.

## Wire shapes

| Surface | Shape |
| --- | --- |
| Join approve / deny / block | `two:voice:join-approve:<room_id>:<request_id>` (etc.) |
| Kick yes / no | `two:voice:kick-yes:<vote_id>` (etc.) |
| Name custom / restore / modal | `two:voice:name-custom:<room_id>` (etc.) |

Wire details (100-char limit, nonzero decimal ids, strict `None` on garbage /
overlong / zero ids) live in `docs/voice-custom-id-core.md`.

## Routing table today

| Input | Component outcome | Modal outcome |
| --- | --- | --- |
| Any minted `two:voice:` shape | `Unknown` (in-guild) / `Ignore` (out-of-guild) | Same as components |
| Malformed / overlong / zero-id `two:voice:` shape | `Unknown`, never a panic | Same as components |
| `two:lfg:<…>` | `Handled(LfgSignup)` while announcements are on | Same as components |
| `two:self-role:<…>` | `Handled(SelfRole)` while self-roles are on | Same as components |

The `two:voice:` prefix never collides with `two:lfg:` or `two:self-role:`
(the codec guarantees this; `no_collision_with_lfg_or_self_role_namespaces`
in `crates/core/tests/voice_custom_id.rs` pins it from the codec side), so
the future V-runtime slice can dispatch on prefix without overlap. Voice
shapes stay `Unknown` regardless of feature gates — unlike lfg/self-role ids,
which become `Ignore` when their gate is off — because no gate owns them yet.

## Acceptance

`crates/core/tests/router_voice_prefix.rs` pins this boundary against the
existing public APIs only
(`InteractionRouter::route_component` / `route_modal`,
`join_custom_id` / `kick_custom_id` / `name_*_custom_id`,
`parse_voice_custom_id`):

1. Every minted voice shape routes to `Unknown` — never `Handled` by the
   lfg/self-role handlers, never `Ignore` in-guild.
2. `two:lfg:` / `two:self-role:` ids still route to their own handlers (no
   regression), on both paths.
3. Malformed / overlong / zero-id voice shapes are `Unknown`, never a panic.
4. Modal submits follow the same table as components, including the guild
   fence (`Ignore` outside the configured guild).

This document describes current behavior and changes none of it. When the
V-runtime slice wires voice dispatch, it updates the router, this table, and
the acceptance test together.
