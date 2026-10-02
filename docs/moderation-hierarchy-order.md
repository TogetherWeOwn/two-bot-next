# Moderation hierarchy refusal order

Card: TOG-12626. Parity gate: `assert_moderation_allowed` in
`crates/core/src/moderation.rs` (legacy `assertModerationAllowed`; see
`docs/parity.md` §1, member-target moderation policy).

## Refusal order

For member-targeted verbs (`ban`, `tempban`, `kick`, `timeout`, `warn`):

1. Actor permission — `ActorMissingPermission`.
2. Target presence — `MissingTarget`.
3. Self-moderation — `TargetSelf`.
4. Target protection — guild owner, Owen, bot, staff role
   (`TargetGuildOwner` / `TargetOwen` / `TargetBot` / `TargetStaffRole`).
5. Bot hierarchy — `BotHierarchy` (evaluated before the actor side).
6. Actor hierarchy — `ActorHierarchy`.

Equal-or-above role positions refuse on both hierarchy sides.

Channel verbs (`purge`, `slowmode`, `lockdown`, `unlock`) skip steps 2–6
entirely: with the actor permission held, they allow regardless of targets
or hierarchy positions.

## Key pins

- When both hierarchy comparisons fail on one request, the error is
  `BotHierarchy` — the bot side is checked first.
- Permission, target-presence, self, and protection refusals all precede
  both hierarchy checks, even when the hierarchy positions would also fail.
- Channel verbs with no target pass hierarchy entirely, and a present
  target (even protected or higher-ranked) is ignored.

## Acceptance

`crates/core/tests/moderation_hierarchy_order.rs` pins each row above with
simultaneous conditions: both hierarchies failing at once, or a
pre-hierarchy refusal combined with failing hierarchy positions. The
single-failure rows stay pinned inline in `moderation.rs`
(`policy_refusals_match_legacy`) and are not repeated there.

Run:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test moderation_hierarchy_order
```
