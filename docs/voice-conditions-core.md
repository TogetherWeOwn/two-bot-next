# V6b template condition evaluator core

`two_bot_core::voice_conditions` is an original, pure implementation derived
only from [the voice-room specification](voice-rooms.md#v6-conditionals-and-styling).
It plugs into the V5 renderer through `ExtensionPolicy::conditional` and needs
no `db` feature, Discord wire types, clock or external I/O.

## Domain contract

- `{{cond ?? yes}}` and `{{cond ?? yes // no}}`. The node splits at the first
  top-level `??`, then at the first top-level `//` after it. A separator inside
  nested syntax (`[[a//b]]`, another `{{..}}`) never splits the node. A node
  without `??` stays literal; so does a node nested deeper than
  `MAX_CONDITION_NESTING` (16).
- Conditions resolve innermost first. A `{{..}}` inside the condition part
  selects its branch, and the selected source becomes part of the outer
  condition text. Condition text is never rendered: name tokens stay as
  source, so `@@owner@@ = Alex` is false, and random syntax in a condition
  consumes no picks. Name tokens in the selected branch are substituted after
  the condition resolves.
- Condition text is trimmed and matched case-insensitively for keywords,
  counters and calendar names. Branch text keeps its spacing; the V5 name tail
  trims the final name and applies the fallback when nothing remains.
- `parse_condition` returns a typed `Condition`. Anything it cannot type is
  `Condition::Unknown`, which is false: unknown keywords, a missing right side,
  a second comparison (`1 < 2 < 3`), `!` prefixes and conditions longer than
  `MAX_CONDITION_BYTES` (256).
- Comparisons `< > <= >= = !=` take an integer literal or a numeric operand on
  each side, including token against token. Numeric operands are the
  `@@num@@`, `@@num_others@@`, `@@num_live@@`, `@@num_playing@@`, `@@limit@@`,
  `@@slots@@`, `@@party_size@@`, `@@hour@@`, `@@room_minutes@@`, `@@room_tier@@`,
  `@@game_minutes@@` and `@@game_tier@@` tokens, plus `$#`/`$0..#` for the
  room number. Each token takes the value the V5 renderer gives it. A blank
  value (`@@slots@@` with no limit) makes the comparison false for every
  operator. `##`, `+#` and name tokens are not numeric.
- `WEEKDAY` and `MONTH` also compare by position (Monday = 1, January = 1)
  against a number or a full English name: `MONTH:September`,
  `WEEKDAY >= 6`, `MONTH < June`. Position follows the guild-offset clock.
- Keywords, with the facts they read:

  | Keyword | True when |
  |---|---|
  | `PLAYING` | the owner has a game activity |
  | `LIVE`, `LIVE_DISCORD`, `LIVE_EXTERNAL` | the owner is live on either source, on Discord, on an external platform |
  | `ANY_LIVE` | any member is live on either source |
  | `ROLE:id`, `ANY_ROLE:id` | the owner has the role; any member has it |
  | `MEMBER:id`, `OWNER:id` | the user is in the room; the user owns it |
  | `OWNER` | the owner is in the room |
  | `GAME` | a game title is shown (one leader or a two-way tie) |
  | `GAME:x`, `GAME=x`, `GAME!=x` | a shown title contains `x`; equals it; none equals it (case-insensitive) |
  | `PLAYERS` | `@@num_playing@@` is above zero |
  | `MAX` | the shown party has a maximum and has reached it |
  | `RICH` | any member reports a rich-presence party |
  | `FULL` | a limit is set and the member count has reached it |
  | `PRIVATE` | the room is private and not a standalone channel |
  | `WEEKEND`, `WEEKDAY` | Saturday or Sunday; Monday to Friday |
  | `MORNING`, `AFTERNOON`, `EVENING`, `NIGHT`, `LATE_NIGHT` | local hour in 05–11, 12–16, 17–21, 22–01, 02–04 |

- Bare `MONTH` names no fact and is false. The shown party follows the V5
  `@@party_size@@` rule: the largest party, none on a three-way tie.
- Truth comes from `ConditionFacts` (IDs, roles, per-source live flags, shown
  titles, privacy) and the `RoomContext` the renderer already holds (counts,
  limit, room number, parties, channel kind, clock).
- Branches render with `Evaluation::evaluate_selected_branch`, so random picks
  inside, before and after a conditional stay stable when the condition flips.

## Composition

`Conditions` is a complete `ExtensionPolicy` that leaves `""mode:text""`
literal. A parent V6 policy owns both extension points: its `conditional`
delegates to `Conditions::evaluate`, and its `styled` applies the V6a style
chain. Branches then style through the parent policy.

## Residual parent integration (not parity evidence)

- The composed policy is `voice_template::TemplateExtensions`
  ([voice-template-core](voice-template-core.md)); it passes the shared corpus,
  styling cases included. `voice_naming::majority_games` gives the shown titles.
- The runtime fills `ConditionFacts` from Discord state: owner and member IDs,
  role IDs, presence activities, both live sources and the room's privacy.
- On malformed input the node split can differ from the V5 reservation split:
  an unpaired style quote, or an unclosed construct wrapping a nested
  conditional. Random stability still holds, because the renderer reserves
  the larger of the policy's consumption and its own branch count.
- Spacing around `??` and `//` is unspecified upstream; this core keeps
  branch text as written.

No live Discord rename is performed or verified by this component's tests.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_conditions
```

Table tests cover every keyword, operator and counter, the unknown and bound
cases, the nested role → live → default chain, innermost-first resolution and
pick stability over 64 seeds. A corpus adapter maps
`tests/voice_templates/corpus.json` contexts to `RoomContext` and
`ConditionFacts`, asserts all 48 exact conditional cases and pins the chosen
output of each deferred condition probe. A property test checks that random
condition syntax renders a deterministic, non-empty name of at most 100
characters. No network, Discord, database, credentials or sleep is needed.
