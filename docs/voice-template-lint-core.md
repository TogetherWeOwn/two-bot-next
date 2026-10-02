# V7b template lint and six-scenario preview core

`two_bot_core::voice_template_lint` is an original, pure implementation derived
only from [the voice-room specification](voice-rooms.md#v12-template-assistant-optional-config-gated)
and its [template admin](voice-rooms.md#v7-template-admin-aliases-nicknames-inspection)
and [conditional](voice-rooms.md#v6-conditionals-and-styling) sections. It
wraps the V5 parser and evaluator in `voice_naming` without changing them and
needs no `db` feature, Discord types, clock or external I/O.

## Domain contract

- `preview(source, &policy)` renders the template in the six fixed
  `Scenario`s, in `Scenario::ALL` order, through the normal `render`
  pipeline (trim, 100-character cap, fallback). Each `ScenarioRender` holds
  the final name and whether the fallback name was used.
- `lint(source, &policy)` returns a `LintReport` of typed `Finding`s:
  - `ParseError(Unclosed(construct))` at the opener of each `@@`, `<<`, `[[`,
    `__`, `{{` or `""` construct the parser keeps as literal text. A valid
    block swallowed by an outer failure is not reported; only the root cause
    is.
  - `ParseError(TooLong)` / `ParseError(TooDeep)` at byte 0 when the whole
    template is literal because it exceeds 4096 bytes or 64 nesting levels.
  - `UnknownToken` at each `@@name@@` outside `KNOWN_TOKENS`, including tokens
    in conditions, which never render but may be evaluated by a policy.
  - `EmptyRender { scenarios }`, one finding listing every scenario whose
    name falls back.
  - `ConditionNeverMatches` at a `{{` block whose condition the policy
    evaluates as false in all six scenarios.
- Parse errors and unknown tokens are `Severity::Error`; empty renders and
  never-matching conditions are `Severity::Warning`. Errors come first in
  source order, then the empty-render finding, then the condition warnings.
- Both functions are generic over `ExtensionPolicy`. A condition's truth is
  probed by rendering it with private-use marker branches: output with only
  the "no" marker is false; anything else is true or unknown. Under
  `PassthroughExtensions` blocks stay literal, so no condition is reported,
  and V6 policies need no change to this module.
- Positions carry the byte offset (a character boundary) and the character
  offset. Findings inside a conditional whose branch split cannot be
  reproduced exactly are anchored at that block's `{{`.

### Bounds

- At most `MAX_FINDINGS` (16) findings; more set `truncated`.
- `excerpt` echoes at most `EXCERPT_CHARS` (24) characters of the input from
  the finding's position, plus `…` when cut. `message()` is fixed text of at
  most `MAX_MESSAGE_CHARS` (120) characters and never includes the input.
- Work is bounded: at most 64 isolated re-parses of unclosed constructs and 32
  conditional blocks per lint, each over at most 4096 bytes. Reaching either
  bound sets `truncated`.

### Scenario fixtures

All fixtures are temporary channels in UTC with no named lists, an empty
fallback (so `Voice Room` is used), `SCENARIO_SEED` (`0x5CE7_A210_0000_0007`)
for every random pick, and the owner present as the first member. The game
title comes from `resolve_majority_game` with default options.

| # | Scenario | Room | Owner | Members (activity) | Extra state | Time (UTC) |
|---|----------|------|-------|--------------------|-------------|------------|
| 1 | solo, no game | 1 | Avery | 1 (idle) | — | Mon 2026-01-05 09:00 |
| 2 | three in a game | 2 | Blake | 3 (Apex ×3) | — | Sat 2026-03-07 21:00 |
| 3 | owner streaming | 3 | Casey | 2 (Chess, idle) | 1 live, stream `Ranked grind` | Wed 2026-05-13 18:00 |
| 4 | game with party info | 4 | Devon | 4 (Apex ×4) | party 3/4, `In Match`, `Ranked` | Sun 2026-07-12 15:00 |
| 5 | nearly full | 5 | Emery | 4 (Chess ×3, idle) | limit 5 | Fri 2026-09-18 23:00 |
| 6 | locked | 6 | Finley | 2 (idle ×2) | limit 2; original creator Avery | Tue 2026-11-10 02:00 |

Repeated calls give byte-identical output: nothing reads a clock, the
environment or a random source.

## Residual parent integration (not parity evidence)

- The V7 template panel and the V12 assistant own presentation: showing
  previews and findings, any confirmation before saving, permission checks
  and audit logging. They should show `excerpt` as untrusted text with
  mentions disabled.
- `RoomContext` has no privacy field, so the locked fixture is modelled as a
  limit equal to the headcount; a V6 `PRIVATE` condition depends on how its
  policy maps that state.
- Six fixed states cannot satisfy every condition. Role, member, month and
  date conditions may be reported as never matching even though they can
  match in a real room; the finding is a warning for that reason.
- Fixtures carry no named lists, so `[[list:name]]` renders empty here.
- `KNOWN_TOKENS` mirrors the evaluator's token list. A test fails if a listed
  token never renders, but a token added to the evaluator must also be added
  here.
- The `TooDeep` check is a heuristic: it fires when the parse is entirely
  literal and the template has at least 64 nesting openers, which can also
  flag a template that is merely full of unclosed openers.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_template_lint
```

Each finding class has positive and negative tests, with exact positions for
nested and multi-byte cases. A test-only conditions policy covers
never-matching conditions and policy-dependent empty renders. Golden tests pin
the preview names of three templates and the fixture facts. Property tests
over random construct fragments pin finding bounds, positions, excerpts,
ordering and determinism. No network, Discord, database, credentials or sleep
is needed.
