# V12 template-assistant config gate and command shape

`two_bot_core::voice_assistant` is an original, pure implementation derived
only from [the approved voice-room specification](voice-rooms.md#v12-template-assistant-optional-config-gated).
It requires no `db` feature, Discord wire types, clock, HTTP client or external
I/O. Endpoint and credential are held in `Secret<String>`.

## Inputs and decisions

- `AssistantConfig::from_map` is the whole gate read. It returns `Some` only
  when `TWO_ASSISTANT_ENDPOINT` trims to a non-empty `http://`/`https://` URL
  without control characters that fits `MAX_ENDPOINT_CHARS` (2048), and
  `TWO_ASSISTANT_API_KEY` supplies a non-blank, control-free credential.
  Credential controls are checked before trimming, so even a trailing CR/LF
  refuses the binding. Other credential key names are not aliases.
  Missing or invalid input returns `None` (disabled), the same fail-closed
  posture as the V1 `VoiceGates`. Over-long endpoints are refused, never cut.
- Empty authorities, URL userinfo (including an empty username), all queries
  and fragments are refused even with a separate valid binding. Refusing
  queries/fragments wholesale avoids depending on a provider's credential
  aliases or percent-decoding rules; even empty ones are refused. Put the
  model in `TWO_ASSISTANT_MODEL`, not the URL.
  This is credential-channel refusal, not the transport destination/HTTPS
  policy required by TA-01.
- `TWO_ASSISTANT_MODEL` trims into the config but never gates it. A blank
  model is refused at call time by the V12b `ModelName` check
  (`voice_assistant_request`), never by truncating to a different model.
- `api_key()` returns only `&Secret<String>`. Exposure belongs solely at a
  future authorization-header boundary. No request option, URL, body,
  dashboard value, diagnostic, log or reply may carry the credential.
  `endpoint()` preserves the existing transport seam; never format its raw
  result. Config Debug redacts the endpoint, credential and supplied model,
  including pretty/nested formatting.
- `assistant_commands()` is the `/templateassistant` definition: admin-gated
  (Manage Guild) with one required `request` option bounded by the V12b
  `MAX_REQUEST_CHARS` limit. The admin's locale arrives in the interaction
  itself, so no locale option is needed; member names, presence and IDs have
  no option to travel in.
- `assistant_command_set(voice, assistant)` produces the command only when
  both gates are on: the V1 voice gate (`TWO_VOICE=1`) and the assistant gate
  (`Some`). Either off means an empty set. The runtime does not publish this
  command until its handler lands; a pure-core definition is not wiring.

## Replay and classification contract

The same environment map always produces the same gate decision. All three
keys are `SettingClass::EnvOnly` in `crates/core/src/settings.rs` and described
in `crates/core/tests/fixtures/reference_settings.json`. None is dashboard
storable. Endpoint/model retain their existing non-secret `FORWARDED_FLAGS`
classification in `wrangler/src/container-env.ts`; never place a credential in
those values. `TWO_ASSISTANT_API_KEY` is `NOT_FORWARDED`: a Container secret
requires its own explicit `containerEnvVars` line, never a flag entry. That
Worker secret delivery and the real transport are not added here, so an
unbound Container remains disabled. `TWO_ASSISTANT_TOKEN` and
`TWO_ASSISTANT_SECRET` are non-forwarded acceptance decoys, not bindings.
The reference-docs golden (`docs/configuration.md`) regenerates from the
catalog. The drift test fails on any new `TWO_*` literal in neither list.

## Residual parent work

Before adding the handler or HTTP transport, satisfy the
[template-assistant security gates](command-wiring-security.md#templateassistant):
HTTPS to an approved environment-fixed destination, no redirects, actual
transport authorization binding, runtime authority and per-user limits. The
pure-core credential gate does not establish runtime compliance.

The per-guild monthly-cap DB column (the V12a `voice_assistant_cap` ledger
consumes the persisted row), the endpoint call with the validated V12b
payload, the V12c six-scenario validation before the admin sees output and
the Apply/Refine/Cancel flow remain separate work. Until the handler lands,
the runtime excludes `/templateassistant` and an invocation gets the router's
unknown-command reply. Tests establish domain behavior only, not runtime
parity or staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets --locked -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib voice_assistant --locked
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_assistant_credential --locked
```

The immutable credential acceptance battery pins missing/wrong/blank/control
binding refusal, URL credential refusal, constant redaction, error/log/reply
hygiene, non-storable classification and the absence of a credential flag or
request option. Unit tests additionally pin retained secret access at a
synthetic authorization boundary, encoded query names and normalized userinfo.
No test needs a database, Redis, Discord, network, real credential or staging
identity.
