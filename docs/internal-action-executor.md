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

Construct with the application's `Arc<twilight_http::Client>` and configured
channel-key map. The application's rustls provider must already be installed.
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
The adapter does not share Twilight's limiter; the receiver must enforce its
own per-caller/action throttling before obtaining an execution claim.

One 10-second deadline covers sending, response headers and success-body
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
| `NoEffect(DiscordRejected / RateLimited)` | Discord rejected with 400/401/403/404/405/413/415/422/429 | Terminal `DiscordRejected`; no automatic resend |
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
Tests pin exact route/payload, disabled mentions, UTF-16 ceilings, channel and
capability refusal, missing authentication, confirmed rejections, no resends on
429/5xx/timeouts/disconnects, complete-response deadline, bounded success decode,
safe receipts, and log/Debug/serialization redaction. Only generated non-secret
fixture tokens are used; no environment credentials, live Discord, DB,
staging/production actions, binding changes or deployment are involved.
