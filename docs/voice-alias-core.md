# Voice alias-resolution core (V7d)

Pure matching core behind the `/alias` panel (`docs/voice-rooms.md` §V7):
aliases apply to `@@game_name@@` and to `GAME` conditions.
Implemented in `crates/core/src/voice_alias_core.rs`;
acceptance cases in `crates/core/tests/voice_alias_core.rs`.

## Functions

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

## Minimal table view

The core reads rows through `AliasRow { key, target }` borrowed exactly as
stored — it owns no storage and duplicates none of V7a's table. V7a
([TOG-12015], PR #212, open at the time of writing) owns `AliasTable` with
add/edit/remove plus `/nick`; its `entries()` map row-for-row onto this
view (key/target pairs, keys unique under folding), and its single-hop
chain rule keeps resolution here a pure function of `(input, table)`.
Convergence: when V7a merges, a thin adapter from `&AliasTable` to
`&[AliasRow]` wires the two together with no changes to matching semantics;
until then this core compiles and tests standalone against hand-built rows.
Note the deliberate folding difference: V7a's `fold_name` is
trim + lowercase + NFC (compatibility forms stay distinct for storage
identity), while this core folds with NFKC for matching, so full-width and
ligature spellings resolve onto the stored ASCII keys. The direction composes
safely — NFKC subsumes NFC, so V7a's fold-unique invariant guarantees this
core's uniqueness precondition, and the extra folding is matching-only with
display strings untouched.

[TOG-12015]: https://github.com/TogetherWeOwn/two-bot-next/issues/12015
