# V12b template-assistant request and reply core

`two_bot_core::voice_assistant_request` implements the pure request builder and
reply parser for [`voice-rooms.md` §V12](voice-rooms.md#v12-template-assistant-optional-config-gated).
It has no HTTP client, endpoint configuration, credentials or monthly cap logic.
The V12a cap ledger (`voice_assistant_cap`) owns the 200-builds-per-month limit.

## Request contract

- `AssistantRequest::new(request, guild_templates, no_game_label, locale)` is
  the only constructor. The type has exactly those four private fields, so
  member names, presence, IDs and room state have no field to travel in. A
  blank request is refused with `RequestError::EmptyRequest`.
- Free text is scrubbed before it is bounded. Every run of 15 or more ASCII
  digits becomes `ID`, which covers snowflakes in pasted mentions and V6
  `ROLE:`/`MEMBER:`/`OWNER:` conditions. Redaction runs before truncation, so a
  cut can never leave part of an ID behind.
- Bounds are counted in Unicode scalar values. Each value is redacted, trimmed,
  cut, then trimmed again at the end. The same input always gives the same
  request.

| Input | Bound | Fallback |
| --- | --- | --- |
| request | 2,000 characters | blank is refused |
| guild templates | 20 templates, 400 characters each | blanks and exact duplicates are dropped and do not use up the 20; caller order is kept |
| "no game" label | 100 characters | blank becomes `General` |
| locale | short BCP 47 style tag, at most 35 characters, subtags of 1–8 alphanumerics, first subtag 2–8 letters | anything else becomes `en-US` |
| model name (`ModelName::new`) | 200 characters, no control characters | refused, never cut |

- `chat_completions_json(&ModelName)` returns an OpenAI-compatible
  chat-completions body with exactly two top-level keys, `model` and
  `messages`. It has two messages: the fixed `SYSTEM_PROMPT`, then one user
  message whose content is the JSON object
  `{"request","guild_templates","no_game_label","locale"}`. The model name
  comes from config through `ModelName`.
- The body leaves out sampling, token-limit and response-format parameters,
  because some compatible models reject them. The caller bounds the reply
  instead (below).
- The system prompt tells the model to treat the user values as data. It
  teaches the English V5/V6 syntax and the six preview scenarios, and asks for
  `{"suggestions":[{"template","explanation"}]}`. The explanation should use
  the language the request asks for, otherwise the locale's language. The
  prompt never offers an ID-bearing condition. A test keeps the prompt's stated
  bounds and token list in step with the parser.
- A change to `SYSTEM_PROMPT` changes the golden test and needs a published
  before/after eval.

## Reply contract

`parse_reply(bytes)` returns 1 to 3 `Suggestion`s or a typed `ReplyError`:

| Check | Error |
| --- | --- |
| body over 64 KiB, before parsing | `Oversize { len }` |
| body is not JSON | `NotJson` |
| top-level `error` object without `choices` | `EndpointError` |
| no `choices[0].message.content` string | `WrongShape { at }` |
| content (after one optional Markdown code fence) is not JSON | `ContentNotJson` |
| no `suggestions` array, or an item without string `template`/`explanation` | `WrongShape { at }` |
| empty `suggestions` | `NoSuggestions` |
| more than 3 suggestions (refused, not truncated) | `TooManySuggestions { count }` |
| template blank, over 400 characters, with a control character or line break, with an unclosed or stray delimiter, or naming a token outside `KNOWN_TOKENS` | `InvalidTemplate { index, issue }` |
| explanation over 600 characters | `ExplanationTooLong { index }` |

- Errors carry only static field paths, indexes and counts. They never wrap a
  `serde_json` error, whose message can quote the reply. Display and Debug never
  echo reply text.
- `parse_template_strict` is the check behind "must parse as a V5 template".
  The V5 parser never fails, because malformed syntax stays literal. The strict
  check walks the parsed AST instead. It refuses any delimiter left in literal
  text (`@@ << >> [[ ]] {{ }} "" __`) and any unknown token. A source that
  exceeds the nesting bound parses as one literal and is refused the same way.
  The accepted `Template` is exactly what `voice_naming::parse` returns.
- Templates and explanations are trimmed. Extra keys in the envelope or in a
  suggestion, such as `usage` or `reasoning_content`, are ignored.

## Residual parent integration

The parent still owns all of the following:

- The config gate: V12 is disabled unless an OpenAI-compatible endpoint is
  configured. It also owns the endpoint URL, credential binding and timeout.
- The HTTP call. It must stop reading the response at `MAX_REPLY_BYTES` and
  hand the bytes to `parse_reply`.
- The V12a cap check before each send.
- The six-scenario preview and validation: empty names and conditions that can
  never match. It regenerates a failed suggestion before the admin sees it.
- Apply/Refine/Cancel, and the `/templateassistant` admin permission gate.
- Showing the explanation with mentions disabled.

V12a's branch also defines an `AssistantRequest` and `validate_request_shape`.
Its template type carries a `channel_id`, so it is not the outbound payload.
The parent should build the outbound body only from this module's
`AssistantRequest`, so the "no IDs" rule holds by construction.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_assistant_request
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
```

The tests pin the serialized request for a fixed input, and assert the field
allowlist and the absence of any 15-digit run over arbitrary inputs. They cover
every bound at and over its limit, every `ReplyError` variant with a no-echo
check, and `KNOWN_TOKENS` against the V5 renderer. No network, credential or
endpoint is used.
