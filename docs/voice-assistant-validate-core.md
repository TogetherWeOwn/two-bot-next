# V12 assistant-output validation core

`two_bot_core::voice_assistant_validate` implements the pure scenario
validation for [`voice-rooms.md` §V12](voice-rooms.md#v12-template-assistant-optional-config-gated).
It has no HTTP client, endpoint configuration, credentials, cap logic or
request/reply parsing. The V12b request module (`voice_assistant_request`)
owns the outbound payload and the strict shape check; this module owns the
"validated against all six scenarios" step that runs before the admin sees an
output. The V12a cap ledger (`voice_assistant_cap`) owns the 200-builds-per-month
limit.

## Validation contract

`validate_template(template, extensions)` checks one assistant-produced
template against the six fixed `voice_template_lint::Scenario`s (solo with
no game; three people in a game; owner streaming; a game with party info;
nearly full with a limit; locked) and returns either `ValidatedTemplate`
with the six preview names in `Scenario::ALL` order — exactly what the admin
preview should show — or a typed `TemplateRefusal`:

| Check | Refusal |
| --- | --- |
| unclosed or stray delimiter (`@@ << >> [[ ]] {{ }} "" __`) | `UnbalancedSyntax` |
| a `@@name@@` outside the engine's token list | `UnknownToken` |
| renders empty (fallback used) in at least one scenario | `EmptyName { scenarios }` |
| a condition false in every scenario | `NeverMatchingCondition` |
| lint hit a scan bound (over 32 conditionals) with nothing else found | `TooComplex` |

- The lint runs once and the highest row in that table wins. Errors (syntax,
  tokens) come before warnings because they cause them: `@@nope@@` reports the
  unknown token rather than the empty renders it also causes. Syntax is first
  because raw syntax showing to members is the worst outcome.
- A truncated lint report fails closed: a never-matching condition past the
  lint's scan bound would otherwise go unchecked, so the template is refused
  as `TooComplex` instead of accepted.
- Tokens are always English, so `UnknownToken` is also the English-only check:
  a non-English token name in ASCII (`@@propietario@@`) is refused the same
  way as a misspelling. A name with non-ASCII letters (`@@propriétaire@@`)
  never parses as a token and is refused as `UnbalancedSyntax`. The
  explanation's language is caller-owned and never inspected — this function
  takes the template alone.
- Condition truth comes from the caller's `ExtensionPolicy`, so the same code
  serves the V5 passthrough policy and later condition policies. Under
  passthrough no condition has a known truth value and none is reported.
- Byte/shape hygiene (blank, over-long, multi-line, control characters)
  belongs to the V12b strict reply parser, which runs before this module. A
  blank source still fails here as empty in every scenario.
- Refusals carry only static text plus scenario labels — never template text,
  member names, presence or IDs. The scenarios themselves are fixed synthetic
  fixtures owned by the lint module, so no member data enters any API. The
  caller regenerates a refused output before the admin sees it.

## Residual parent integration

The parent still owns all of the following:

- The config gate: V12 is disabled unless an OpenAI-compatible endpoint is
  configured. It also owns the endpoint URL, credential binding and timeout.
- The V12a cap check before each send, and the V12b request build and reply
  parse around it.
- Calling `validate_template` on every suggestion and regenerating failures
  before showing Apply/Refine/Cancel.
- The `/templateassistant` admin permission gate and the 200-builds/month
  accounting.
- Showing the explanation with mentions disabled.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_assistant_validate
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
```

Each of the three §V12 failure classes has a failing-output refusal test plus
a passing-output accept test across the six scenarios: empty names (stream
title only live in one scenario; owner-plus-number accepted in all six),
never-matching conditions (`PRIVATE` refused; `FULL`/`GAME`/`WEEKEND`
accepted with per-scenario goldens), and unknown tokens (misspelled and
non-English names refused; every engine token accepted). Priority, the
fail-closed scan bound, preview identity and refusal-message bounds are
pinned. No network, credential,
endpoint, database, Discord or staging identity is used.
