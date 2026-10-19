# V12 template-assistant config gate and command shape

`two_bot_core::voice_assistant` is an original, pure implementation derived
only from [the approved voice-room specification](voice-rooms.md#v12-template-assistant-optional-config-gated).
It requires no `db` feature, Discord wire types, clock, HTTP client,
credentials, or external I/O.

## Inputs and decisions

- `AssistantConfig::from_map` is the whole gate read. It returns `Some` only
  when `TWO_ASSISTANT_ENDPOINT` trims to a non-empty `http://`/`https://` URL
  without control characters that fits `MAX_ENDPOINT_CHARS` (2048). Anything
  else — missing, blank, wrong scheme, bare authority, control characters,
  over-long — returns `None` (disabled), the same fail-closed posture as the
  V1 `VoiceGates`. A truncated endpoint would address a different server, so
  over-long values are refused, never cut.
- `TWO_ASSISTANT_MODEL` trims into the config but never gates it: the spec
  disables on a missing endpoint only. A blank model is refused at call time
  by the V12b `ModelName` check (`voice_assistant_request`), never by
  truncating to a different model.
- `assistant_commands()` is the `/templateassistant` definition: admin-gated
  (Manage Guild) with one required `request` option bounded by the V12b
  `MAX_REQUEST_CHARS` limit. The admin's locale arrives in the interaction
  itself, so no locale option is needed; member names, presence and IDs have
  no option to travel in.
- `assistant_command_set(voice, assistant)` publishes the command only when
  both gates are on: the V1 voice gate (`TWO_VOICE=1`) and the assistant
  gate (`Some`). Either off means an empty set — nothing is registered.

## Replay and classification contract

The same environment map always produces the same gate decision. Both keys
are `SettingClass::EnvOnly` in `crates/core/src/settings.rs` (an endpoint
URL and model name must never be dashboard-settable, like
`DISCORD_API_BASE`), described in
`crates/core/tests/fixtures/reference_settings.json`, and forwarded in
`wrangler/src/container-env.ts` `FORWARDED_FLAGS` (non-secret runtime
values; an endpoint credential, if any, travels as its own Container secret,
never through the flag allowlist). The reference-docs golden
(`docs/configuration.md`) regenerates from the catalog. The drift test
fails on any new `TWO_*` literal in neither list.

## Residual parent work

Before adding the handler or HTTP transport, satisfy the
[template-assistant security gates](command-wiring-security.md#templateassistant):
HTTPS to an approved environment-fixed destination, no redirects, credential
binding and redaction, runtime authority and per-user limits. The permissive
pure-core endpoint predicate and command publication do not satisfy these gates.

The per-guild monthly-cap DB column (the V12a `voice_assistant_cap` ledger
consumes the persisted row), the endpoint call with the validated V12b
payload, the V12c six-scenario validation before the admin sees output, the
Apply/Refine/Cancel flow and the endpoint credential binding remain outside
this slice. Publication itself is wired: `InteractionRouter::publish_set`
merges `assistant_commands()` after the voice set when both gates are on.
Until the handler lands, an invocation gets the router's unknown-command
reply. Unit tests establish domain behavior only, not runtime parity or
staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets --locked -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib voice_assistant --locked
```

The unit tests pin the disabled-by-default posture, every refusal class
(blank / non-http / bare-authority / control-character / over-long
endpoints), the trimming behavior, the admin-gated single-option command
shape with the V12b request bound, and the both-gates-on publish rule. No
tests in this slice use a database, Redis, Discord, network, credential, or
a staging identity.
