# V7a game-alias and `/nick` core

`two_bot_core::voice_alias` is an original, pure implementation derived only
from [the approved voice-room specification](voice-rooms.md#v7-template-admin-aliases-nicknames-inspection).
It requires no `db` feature, Discord wire types, clock, store or external I/O,
and does not depend on V1 room lifecycle.

## Alias table

An entry maps a raw activity **key** to the canonical **target** that
`@@game_name@@` and `GAME` conditions see. In the V11 export it corresponds to
`GameAlias { game: key, alias: target }`. `AliasTable` keeps entries in
insertion order and offers `add`, `edit` (new target; key spelling and position
kept, rename means remove then add), `remove`, and `from_entries` (rebuild from
stored or imported pairs by applying `add` in order; the first refusal fails
the load). Refused operations leave the table unchanged.

| Limit | Value |
| --- | --- |
| `MAX_ALIAS_KEY_CHARS` | 100 Unicode scalars after trimming |
| `MAX_ALIAS_TARGET_CHARS` | 100 Unicode scalars after trimming (channel-name ceiling) |
| `MAX_ALIASES_PER_GUILD` | 100 entries |
| `MAX_ERROR_ECHO_CHARS` | 100: the most input any `AliasError` repeats |

Both halves are trimmed, must be non-empty, and refuse control characters (Cc,
which covers every C0/C1 newline), U+2028/U+2029 and bidi embedding, override
and isolate controls. Keys are unique under `fold_name`, so `PUBG`, `pubg` and
` Pubg ` are one key.

**Chain rule:** every target resolves to exactly itself. A target may fold to
another entry's key only when that entry's target is the identical text, and a
key may fold to another entry's target only when both targets are identical.
Otherwise `TargetIsKey` or `KeyIsTarget` refuses the change. This allows a case
correction (`APEX LEGENDS` → `Apex Legends`) alongside other spellings pointing
at the same `Apex Legends`, and refuses every two-hop chain and cycle. Resolution
is therefore a single hop and idempotent.

Errors echo at most one already-validated key, never raw input, so no message
repeats more than `MAX_ERROR_ECHO_CHARS` of input or any control character.
Stored keys can still contain `@` text, so the runtime sends replies with
mentions suppressed.

## `resolve_game(raw_activity_name, &aliases)`

Matching uses `fold_name`: trim surrounding whitespace, lowercase, then NFC.
Canonically equivalent spellings (precomposed `é` versus `e` plus U+0301)
match. Compatibility forms are **not** folded (no NFKC): full-width letters,
ligatures and `™` stay distinct, as does interior whitespace. Only a whole-key
match counts, never a prefix or substring. A match returns the stored target;
anything else returns the input unchanged, untrimmed. Choosing the majority
game across a room is V5's job: resolve each activity before counting.

## `/nick`

- `validate_nick(raw)`: trims, then requires 1 to `MAX_NICK_CHARS` (32)
  Unicode scalars. Refuses the same control characters as aliases, `@everyone`
  and `@here` anywhere in any case, and `<@digits>`, `<@!digits>` and
  `<@&digits>` mention syntax. Nothing else is normalised. `NickError` never
  echoes input.
- `parse_nick_command(raw)`: the keyword `reset` (trimmed, ASCII
  case-insensitive) is `NickUpdate::Reset`; anything else is
  `NickUpdate::Set(validate_nick(raw)?)`. Nobody can be named "reset".
- `owner_display(nick, display_name)`: the stored nick wins when present and
  still valid; otherwise the display name. A stored value that no longer
  validates is ignored rather than rendered.

## Residual parent work

The `/alias` panel, `/nick` slash routing, persistence of the table and of
per-member nicks, V5 majority-game selection and template rendering, V6 `GAME`
comparisons, and V11 import mapping stay on the V7 parent. Unit tests establish
domain behaviour only, not runtime wiring or staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_alias
```

The fixture has table tests for every refusal and limit edge, case and NFC
matching, the no-NFKC choice, pass-through, reset and fallback display. Three
proptest properties cover the rest. Resolving a canonical name returns it.
No add, edit or remove sequence creates a chain or cycle, and refusals change
nothing. A validated nick never contains a control character. No test uses a
database, Redis, Discord or a staging identity.

## V7d alias-resolution core

Pure matching core behind the `/alias` panel (`docs/voice-rooms.md` §V7):
aliases apply to `@@game_name@@` and to `GAME` conditions.
Implemented in `crates/core/src/voice_alias_core.rs`;
acceptance cases in `crates/core/tests/voice_alias_core.rs`.

#### Functions

- `normalize_game_name(raw) -> String`: NFKC compatibility folding, trim,
  then case-fold. Matching-only; display strings are never touched — callers
  render the stored target or the raw detected name.
- `resolve_alias(normalized, rows) -> Option<canonical>`: exact match first,
  then longest key that prefixes the input, else `None` (caller falls back
  to the raw detected name). The input must already be normalized; keys are
  normalized the same way, so stored keys may use any case or spelling.
- `applies_to(token) -> bool`: `true` for the `@@game_name@@` token name
  (`game_name`, case-insensitive) and the `GAME` condition head (`game`,
  case-insensitive); every other token is refused.

#### Minimal table view

The core reads rows through `AliasRow { key, target }` borrowed exactly as
stored — it owns no storage and duplicates none of V7a's table. V7a
([TOG-12015], PR #212, merged as `voice_alias`) owns `AliasTable` with
add/edit/remove plus `/nick`; its `entries()` map row-for-row onto this
view (key/target pairs, keys unique under folding), and its single-hop
chain rule keeps resolution here a pure function of `(input, table)`.
Convergence: now that V7a has merged, a thin adapter from `&AliasTable` to
`&[AliasRow]` wires the two together with no changes to matching semantics;
today this core compiles and tests standalone against hand-built rows.
Note the deliberate folding difference: V7a's `fold_name` is
trim + lowercase + NFC (compatibility forms stay distinct for storage
identity), while this core folds with NFKC for matching, so full-width and
ligature spellings resolve onto the stored ASCII keys. The direction composes
safely — NFKC subsumes NFC, so V7a's fold-unique invariant guarantees this
core's uniqueness precondition, and the extra folding is matching-only with
display strings untouched.

[TOG-12015]: https://github.com/TogetherWeOwn/two-bot-next/issues/12015
