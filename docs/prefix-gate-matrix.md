# Prefix text-trigger gate matrix

Parity §1 #23: the `!<trigger>` prefix surface (`TWO_TEXT_COMMANDS=1`,
builtin names excluded). Acceptance map for
`crates/core/tests/prefix_gate_matrix.rs`, which pins the gate and
builtin-exclusion matrix against the existing public
`two_bot_core::feature_commands::parse_prefix_trigger` API. Parser shape rows
(unicode/whitespace edges, case fold, 32-char bound) live in
`crates/core/tests/prefix_trigger.rs` and are not repeated here.

## Gate

`text_commands_enabled` is the `FeatureGates::text_commands` flag
(`TWO_AUTOMATIONS=1` AND `TWO_TEXT_COMMANDS=1`). Off rejects everything,
including otherwise-valid triggers. On resolves the first token per the
parser rules.

## Matrix

| # | Input shape | Gate | Expected |
|---|---|---|---|
| 1 | Any trigger, including valid non-builtin (`!faq`, `!rules-day_2`) | OFF | refuse |
| 2 | Valid non-builtin (`!faq`, `!rules-day_2`, `!welcome`) | ON | bare name |
| 3 | Every retained builtin slash name (core + scorecard + automation + announcement + moderation) | ON | refuse |
| 4 | `!` not in first position (` !faq`, `x!faq`, `say !faq please`, `!!faq`) | ON | refuse |
| 5 | Multi-word input (`!welcome to the server`) | ON | first token only; builtin first token still refused |

## Notes

- Builtin exclusion means prefix traffic can never shadow a slash command:
  a folded trigger colliding with a builtin name is rejected.
- Stored triggers always match the legacy shape (`TRIGGER_PATTERN =
  /^![a-z0-9_-]{1,32}$/`), so a parse miss is a store miss. Gateway wiring
  and custom-command storage are out of scope (follow-ups under TOG-10080);
  the caller resolves the returned name against its store.
