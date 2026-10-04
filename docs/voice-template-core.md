# Voice template core (V6 composition)

`two_bot_core::voice_template` composes the V6 condition evaluator
([voice-conditions-core](voice-conditions-core.md)) and the styling library
([voice-style-core](voice-style-core.md)) into the V5 naming engine, written
only from [the voice-room specification](voice-rooms.md#v6-conditionals-and-styling).

## Behaviour

- `TemplateExtensions::new(&facts)` is the V6 `ExtensionPolicy`: `{{cond ??
  yes // no}}` goes to `Conditions::evaluate`, and `""mode:text""` renders its
  body (tokens, choices and nested conditionals) and then applies the mode chain.
- `voice_template::resolve_room_name(template, ctx, facts, raw_name)` keeps the
  V5 fallback contract: a blank or oversized template keeps `raw_name`, an
  empty render falls back to it, and the output is never empty and never over
  100 characters. The V5 `voice_naming::resolve_room_name` still renders both
  extensions literally.
- Pipeline order is unchanged: conditionals, tokens, styling, then trim,
  truncation by characters and fallback. Truncation counts styled characters,
  so a font never splits a code point.
- Random picks keep their positions. A styled body reserves its picks whatever
  its modes do, and a conditional reserves both branches, so a flip never
  re-rolls a later `[[a/b]]`.
- `rand` is seeded from the stored room seed alone, in its own domain, so it
  is stable across renames and membership changes and independent of the
  choice dice.
- `voice_naming::majority_games` returns the titles `@@game_name@@` shows
  (one, both on a two-way tie, none otherwise). Feed it to
  `ConditionFacts::games` so `GAME` always agrees with the visible title.

## Residual runtime work

The runtime that renames rooms fills `ConditionFacts` from Discord state
(owner and member IDs, role IDs, activities, both live sources, privacy) and
calls `voice_template::resolve_room_name`. No runtime, Discord or staging
behaviour is performed or verified by these tests.

## Hermetic verification

```sh
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo test -p two-bot-core --test voice_template --test voice_template_corpus --locked
cargo test -p two-bot-core --doc voice_template --locked
```

`voice_template` covers every styling mode through the engine, exact goldens
for the case, word and font modes, chains and unknown modes, `rand` stability,
the nested fallback chain (role, then live, then default), random-position
stability and the fallback contract. The shared corpus now runs on the full
policy, and every non-deferred case must pass. No test uses a database, Redis,
Discord or a staging identity.
