# Interaction reply lifecycle

The router's async entry point is `two_bot_discord::dispatch_interaction`.
`route_interaction` remains the pure selection API; by itself it does not send
replies. The current gateway adapter still models `InteractionCreate` as an
unmodelled event: production feature/gateway wiring is a separate slice, not a
second dispatcher hidden in this change.

## Adapting existing feature functions

Construct `InteractionReplyTransport::new(&executor, &interaction)` and pass it
with the router, interaction and `DispatchOptions` to `dispatch_interaction`.
The closure receives the selected `RoutedInteraction` and a `ReplySession`:
call the existing feature function, then map its outcome to
`InteractionReply::new(content, ephemeral)`. Existing `InteractionHandler`
registration and feature function signatures are unchanged. No DB or REST
client is added to core routing.

Set `DispatchOptions.ephemeral` from the command/component's visibility policy
before execution. Do not infer it from user input. `reply_policy` defaults to
2 seconds; `ReplyPolicy::new(Duration::from_millis(...))` overrides it and
rejects budgets of 3 seconds or more. This is the time to **start** an ACK, not
a guarantee of delivery: callers must leave network headroom and dispatch
promptly after receiving the event. DB lookups performed before dispatch are
outside the timer. Handlers must yield and offload blocking work; an async
timer cannot interrupt synchronous CPU work.

Handlers that ACK early must use `session.respond(...)` or
`session.defer(ephemeral)`, not raw HTTP. The session serializes them with the
auto-defer timer, so exactly one initial callback is sent. On completion the
wrapper edits an acknowledged original instead of sending another callback.
It does not cancel the handler when the budget expires or detach tasks.

## Reply and error contract

| State at completion | Success | Error/panic |
| --- | --- | --- |
| No ACK | type 4 callback | type 4 ephemeral callback |
| Ephemeral type 5 defer | PATCH original | PATCH original (still private) |
| Public type 5 defer | PATCH original | DELETE public placeholder, POST ephemeral followup |
| Handler already replied | PATCH original | POST ephemeral followup |

Discord fixes visibility at the initial ACK; PATCH cannot turn a public
original ephemeral. A private success returned after a public defer also
replaces the placeholder with an ephemeral followup. For success after any
other ACK, visibility remains that of the original, so the adapter must use a
consistent policy. Errors after an early reply never overwrite that public
message with error content.

Every handler `Err` and unwind panic (both future construction and polling)
logs `reference` and the internal error at ERROR level. The only error text
sent to Discord is `Something went wrong (ref XXXXXXXX)`; the eight hex digits
are random and contain no interaction token or user data. Transport failures
are returned to the caller, not falsely reported as successful replies.
Failed/uncertain sends are not retried: Discord may already have received the
ACK. A failed callback abandons this wrapper's pending handler rather than
performing blind retries. No delivery guarantee is made while Discord is
unavailable. Panic isolation needs `panic = "unwind"`, including the release
profile; it cannot isolate process aborts or double panics in destructors.

Unknown slash names, disabled/missing custom rows and unknown component/modal
IDs inside the configured guild receive the same ephemeral
`This interaction is no longer available.` reply. Guild fences, permissions,
known disabled component gates, ping and autocomplete retain their existing
behaviour. Routing does not authorize any feature side effects.

Text replies cap content at 2000 Unicode scalars and suppress all mentions.
The transport carries the selected interaction's IDs/token and deliberately
has no Debug implementation. Callback, edit, deletion and followup operations
use the existing ActionExecutor's unpaced interaction lane, not moderation
pacing or retry loops.

## Legacy source parity

Read against `TogetherWeOwn/two-bot` main on 2026-09-30:

- `src/discord/onboarding.ts` defers with `MessageFlags.Ephemeral` then calls
  `editReply({ content })`. This corresponds to callback type 5, flags 64,
  followed by PATCH `/webhooks/{application}/{token}/messages/@original`.
- `test/unit.interaction-reply.test.ts` does **not** test generic error copy or
  universal auto-defer. It tests `test/helpers/interactionReply.ts`: an edit of
  the selected interaction token is final delivery, accepting either literal
  `@original` or percent-encoded `%40original`. A type 5 deferred callback alone
  is not completion. GETs, other tokens and `@original-extra` are ignored.
- That test intentionally returns captured empty content rather than treating
  it as missing, and selects the **first** matching edit: a later correct edit
  must not conceal a wrong first reply. The new wire regression likewise
  asserts the first callback/edit and the exact selected token/original path.
- Intentional improvements, not claims of legacy byte parity: deadline-driven
  rather than unconditional defer; correlation-only generic errors; private
  error followups for already-public ACKs; explicit replies for unknown IDs.
  Feature-specific refusal texts remain unchanged.

## Evidence

`crates/core/tests/reply_lifecycle.rs` uses paused time and an in-memory
transport: slow/fast handlers, configurable budget, early ACK races, errors
with matching captured logs, private/public deferred failures, construction
and polling panics followed by a healthy interaction, early-reply failure,
uncertain transport failure and Unicode limits.

`crates/discord/tests/interaction_routing.rs` sends through the real executor
and Twilight request builders into `MockRest` on loopback. It asserts type 5
then PATCH original for both visibility policies, mention suppression,
consistent unknown/refusal callbacks, guild fencing and the DELETE/private
followup sequence. These tests do not contact production/staging or a DB.
