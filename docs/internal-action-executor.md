# Internal announcement executor

`two_bot_discord::internal_actions::AnnouncementExecutor` is a callable effect
adapter, not an HTTP receiver, deployment or authorization boundary. It does not
change runtime flags, expose a listener, claim/audit an operation, or release any
HMAC provisioning HOLD. Future receiver work owns those steps.

## Capability and validation

`SUPPORTED_ACTIONS` and `AnnouncementExecutor::supports` expose exactly
`announcement.post`. Every other action is refused without HTTP, including other
core phase-1 defaults. The receiver must intersect its enabled/authorized actions
with this capability; the core's 19-verb catalogue is not executor parity.

Construct with the application's `Arc<twilight_http::Client>`, configured
channel-key map and the bot token's one `CooldownGovernor`. The application's
rustls provider must already be installed.
The map is held privately, starts empty if unconfigured, and is the only source
of target channels. A request cannot override it using a raw channel ID.

`execute(action, body)` reuses core `validate_announcement`: non-empty strings,
known `channel_key`, and at most 2,000 UTF-16 units of message `body`. Mapped
snowflakes must additionally be canonical, nonzero, u64-representable Discord
IDs. The adapter drops core `ActionError` text because an unknown key is echoed
in that error. Only fixed refusal enums leave the adapter.

Twilight `create_message(...).content(...).allowed_mentions(...)` builds and
validates the actual JSON POST to `/api/v10/channels/{mapped-id}/messages`.
Allowed mentions are explicitly empty, overriding any client defaults. Text is
preserved, but cannot ping users, roles or everyone through this endpoint.
Arbitrary input fields, including OAuth material, are never forwarded.

## Single attempt and safe results

The adapter uses `TryIntoRequest`, never awaits Twilight's `ResponseFuture`,
and sends that validated request through a separate Hyper transport. Twilight
otherwise retries 429 internally even with its rate limiter disabled. Hyper's
cancelled pooled-connection retry is also explicitly disabled. There are no
application status retries or redirects. The production origin is fixed to
`https://discord.com`; only module-local test code overrides it with loopback.
The raw transport sends the required `DiscordBot (URL, version)` User-Agent.

The adapter does not share Twilight's limiter. It feeds every
`RateLimited(RateLimitCooldown)` into the `CooldownGovernor` passed at
construction before returning, and consults that governor before every send
(`crates/discord/src/internal_actions/governor.rs`). Build one governor per bot
token and hand clones to every executor, guild worker and other Discord
transport using that token; executor clones share it. `Global` holds every send
with that token; `Channel(id)` conservatively holds all buckets using that
channel major resource (this executor implements just the create-message route,
so no raw provider bucket ID is needed or exposed). A hold starts when the
outcome is recorded and only ever lengthens: a shorter later cooldown never
resets it. If timing is `None`, the scope stays held until independent
reconciliation calls `reconcile(scope)`; there is no invented short default.

While a matching hold is active the executor returns `NoEffect(CoolingDown)`
without HTTP. It refuses; it never queues, sleeps or retries, so repeated 429s
cannot keep an operation alive. The governor is in memory and per process, so a
restart forgets holds. It keeps at most `MAX_CHANNEL_HOLDS` (1,024) channel
holds: expired holds are reclaimed first, and past the ceiling a further channel
cooldown widens to the token-wide hold instead of evicting a live one. Intents
already in flight when a 429 lands are not recalled; this is admission control,
not pacing. Per-caller/action admission throttling stays with the receiver.

Timing uses the longer valid `Retry-After` header/body `retry_after`, in seconds
rounded up to milliseconds, without shortening long waits. Global header/body
signals win over channel signals; ambiguous scope defaults to global. Error
bodies are also bounded by size and deadline; malformed, truncated, oversized or
slow 429 bodies preserve known headers and definite no-effect status. Only typed
scope/IDs/timing leave the adapter, never provider bucket/error text. A 429 does
**not** schedule a retry: the original key remains terminal, and the shared
cooldown applies to later, genuinely new intents.

One 10-second deadline covers sending, response headers and body
collection. Bodies are capped at 64 KiB. A receipt requires HTTP 200/201,
validated string message/channel IDs, and the expected channel. Only those two
IDs are returned; the full Discord message model is never returned or cached.
Logs, Debug and serialization contain only closed enums and validated IDs;
request values, token headers and provider error sources are dropped.

| Outcome | Evidence | Required caller disposition |
| --- | --- | --- |
| `Posted(receipt)` | Successful create with valid IDs and expected channel | `finish(Success { resource_id: message ID, affected: 1 })` |
| `NoEffect(Malformed)` | Local validation refusal, no request sent | Terminal `Malformed` |
| `NoEffect(ActionNotAllowed)` | Unknown verb/key, no request sent | Terminal `ActionNotAllowed` |
| `NoEffect(InvalidChannelConfiguration / LocalConfiguration)` | Invalid mapping/header or missing client authentication, no request sent | Terminal `NoEffect`; repair configuration separately |
| `NoEffect(DiscordRejected)` | Discord rejected with 400/401/403/404/405/413/415/422 | Terminal `DiscordRejected`; no automatic resend |
| `NoEffect(CoolingDown)` | The shared governor holds this channel or the token, no request sent | Terminal `NoEffect`; no automatic resend |
| `RateLimited(cooldown)` | Discord rejected with 429; timing/scope retained safely | The executor already recorded the cooldown in its governor; persist terminal `DiscordRejected`; never resend this key |
| `Unknown(reason)` | Timeout, transport failure, redirect/408/5xx/unrecognized status, malformed/truncated/oversized success or wrong channel | `mark_unknown`; retain claim, require independent reconciliation |

The receiver must authorize, burn the nonce, check capability, and commit its
intent/audit claim **before** invoking this adapter. Only a newly returned
execution claim permits calling it. This adapter is not safe as an unauthenticated
public endpoint. See [the durable store contract](internal-action-store.md).

Persist the typed terminal result and audit before returning a terminal response.
A finalization failure after `Posted` is unknown, not permission to recreate.
Caller cancellation or a crash during execution is likewise unknown. Neither
unknown nor definitive failure releases the idempotency slot: retries replay a
cached failure or require reconciliation, never re-execute the original key.
A genuinely new user intent requires a new key.

## Verification boundary

`cargo test -p two-bot-discord --lib internal_actions` exercises real Twilight
request validation/serialization against an ephemeral loopback HTTP proxy.
Tests pin the required User-Agent, rate-limit header/body timing and global/channel
scope (including absent/invalid timing and slow/broken bodies), redacted buckets,
exact route/payload, disabled mentions, UTF-16 ceilings, channel and
capability refusal, missing authentication, confirmed rejections, no resends on
429/5xx/timeouts/disconnects, complete-response deadline, bounded success decode,
safe receipts, and log/Debug/serialization redaction. Only generated non-secret
fixture tokens are used; no environment credentials, live Discord, DB,
staging/production actions, binding changes or deployment are involved.
