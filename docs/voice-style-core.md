# V6a styling core integration seam

`two_bot_core::voice_style` is an original, pure implementation derived only
from [the approved voice-room specification](voice-rooms.md#v6-conditionals-and-styling).
It requires no `db` feature, Discord wire types, clock, store, or external I/O,
and has no dependency on V1 ([TOG-10093](/TOG/issues/TOG-10093)) or any other
voice slice.

## Inputs and decisions

- `parse_modes("upper+bold")` splits on `+` into one `StyleMode` per segment.
  Matching is ASCII case-insensitive after trimming; `caps` is an alias for
  `upper`, and `<N>w` (ASCII digits followed by `w`) keeps the first `N` words.
  Anything else becomes `Unknown` carrying the raw segment, and leaves the text
  unchanged.
- `apply_mode(&mode, text, seed)` is a total `&str -> String` function. The
  seed only affects `Rand`; every other mode ignores it.
- `apply_chain(&modes, text, seed)` applies modes left to right.
- Case: `upper`/`caps`, `lower`, `title`, `swap`, `scaps` (small capitals;
  lowercase letters only, `x` passes through because Unicode has no small
  capital `x`), `rand` (deterministic per caller-supplied `u64` seed via
  splitmix64; uncased characters pass through without consuming the stream).
- Words and spacing: `spaces` (every character separated by a space), `acro`
  (first character of each whitespace-separated word), `remshort` (drops exactly
  `REMSHORT_STOP_WORDS`: a, an, and, at, by, from, in, is, of, on, or, the, to;
  case-insensitive), `<N>w` (first `N` whitespace-separated words).
- Novelty: `uwu` (`r`/`l` become `w` preserving case after `ove` becomes `uv`),
  `usd` (single-code-point upside-down flip, reversed; unlisted characters such
  as most uppercase letters and the digits 4, 5 and 7 pass through unchanged).
- Unicode fonts: `bold`, `italic`, `bolditalic`, `script`, `boldscript`,
  `fraktur`, `boldfraktur`, `double`, `sans`, `boldsans`, `italicsans`,
  `bolditalicsans`, `mono`. Each font is a contiguous Mathematical Alphanumeric
  Symbols base plus the reserved-gap capitals that live in the Letterlike
  Symbols block (script `B E F H I L M R` and lowercase `e g o`; fraktur
  `C H I R Z`; double-struck `C H N P Q R Z`; italic `h` as Planck constant).
  Fonts without a digit block leave digits unchanged; characters outside
  `A-Z`, `a-z` and `0-9` pass through unchanged.

## Residual parent work

Conditionals stay on [TOG-10097](/TOG/issues/TOG-10097); they need the V5
evaluator ([TOG-10095](/TOG/issues/TOG-10095), in review). The runtime wiring
stays on the parent card, behind its V1 edge: seeding per room, parsing the
`""mode:text""` chain, nesting inside conditionals, enforcing the 100-character
output bound, and authenticating template admin permissions. No runtime, Discord,
or staging behaviour is performed or verified by this component's tests.

## Hermetic verification

```sh
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo test -p two-bot-core --test voice_style --locked
```

The golden table pins at least 40 independently computed (mode, input, expected)
rows covering every mode, the reserved-gap capitals, non-alphabet passthrough,
chains, and unknown modes. Property tests pin `rand` determinism per seed,
character-count preservation for the font modes, seed-independence of every
other mode, unknown-mode identity, and per-segment parse coverage. No tests in
this fixture use a database, Redis, Discord, or a staging identity.
