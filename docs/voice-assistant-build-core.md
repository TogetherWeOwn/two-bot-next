# V12c template-assistant select-and-retry build

`two_bot_core::voice_assistant_build` implements the pure build pipeline for
[`voice-rooms.md` §V12](voice-rooms.md#v12-template-assistant-optional-config-gated).
It turns one admin request into one validated template with its six scenario
previews and explanation, ready to show behind Apply/Refine/Cancel (that flow
lives in a later slice). It has no HTTP client, credential, clock, database,
monthly-cap ledger or Discord wire types. The endpoint call is the injected
[`AssistantTransport`], so the real HTTPS wiring arrives in a later slice
without changing this pipeline.

## Pipeline, in order

1. `AssistantRequest::new` bounds the four allowed inputs (request, guild
   templates, "no game" label, locale). Member names, presence and IDs have no
   field to travel in; a blank request fails as `BuildError::EmptyRequest`
   before any endpoint call.
2. `ModelName::new` checks the configured model. A blank model fails as
   `BuildError::BadModel`, never by truncating to a different model.
3. One deterministic chat-completions body is serialized; every attempt sends
   the same bytes. A test pins that the bodies of repeated attempts are equal.
4. Up to `MAX_BUILD_ATTEMPTS` (3) endpoint calls: each reply is parsed, then
   suggestions are tried best-first against the six-scenario
   `validate_template`. The first suggestion that validates wins with its
   previews and explanation.
5. A refused suggestion set is regenerated — a fresh endpoint call — before the
   admin sees anything. When every attempt's suggestions are refused, the build
   fails with the last refusal.

## Per-suggestion parse

`parse_reply_each` (in `voice_assistant_request`) splits the old all-or-nothing
`parse_reply` into one result per suggestion. Envelope failures (oversize,
non-JSON, error object, wrong shape, empty or over-long suggestion list) and
per-suggestion shape failures (missing fields, over-long explanation) still
fail the whole reply. A suggestion whose template the strict parse rejects is
returned as `Err(issue)` at its own position instead, so the pipeline can try
the next suggestion — best first — before regenerating. `parse_reply` keeps its
signature and delegates, so existing callers are unchanged.

Byte hygiene the strict parse rejects never reaches scenario validation; it
regenerates as the matching refusal:

| Strict issue | Regenerates as |
| --- | --- |
| blank | empty name in every scenario |
| unknown token | unknown token |
| unclosed delimiter, control character | unbalanced syntax (would show raw in the name) |
| over-long source | too-complex (refused, never accepted unchecked) |

## Errors

`BuildError` variants carry only static text, counts and scenario labels. They
never repeat reply text, template sources or member data. A marker test asserts
no reply token appears in any `Display` or `Debug` rendering:

| Variant | When | Retried? |
| --- | --- | --- |
| `EmptyRequest` | blank admin request | never calls the endpoint |
| `BadModel` | blank, over-long or control-character model | never calls the endpoint |
| `Transport` | the endpoint call failed (redacted by the transport) | no — an identical body would get an identical rejection |
| `BadReply` | oversize, non-JSON, error object, wrong shape, no usable suggestion | no — same reason |
| `NoUsableSuggestion { attempts, refusal }` | every attempt's suggestions refused scenario validation | after 3 attempts; `refusal` is the last one seen |

## Transport contract

The implementor POSTs `body` — the exact chat-completions JSON — to `endpoint`
(the full chat-completions URL from configuration; no path is joined here) with
a JSON content type, and returns the raw response body. Implementors must bound
the call with their own deadline and must redact endpoint details from errors
the way the backup HTTP error wraps URLs and reasons: build errors surface to
logs, and URLs can carry credentials. Any authorization header is the
transport's business; this pipeline never sees the credential.

## Residual parent integration

The parent still owns all of the following:

- The real HTTPS transport with the endpoint credential binding and timeout.
- The V12a per-guild monthly-cap ledger check before calling.
- The Apply/Refine/Cancel flow around the returned build.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_assistant_build
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_assistant_request
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
```

The tests use a scripted transport, so no network, database, Discord,
credential or staging identity is used. They cover the happy path (template,
explanation and all six preview names), best-first ordering within one reply,
regeneration with identical bodies, exhaustion with the last refusal, immediate
failures without retry (malformed reply, transport failure, blank request,
blank model), and the no-echo hygiene check.
