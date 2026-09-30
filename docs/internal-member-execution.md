# Internal member executors

The HTTP receiver remains out of scope. `executor::member` implements the two
website join-journey verbs over the shared single-attempt REST transport.

## Contract

- `RoleAssignRequest::validate` resolves a configured role key, not a caller role
  ID. The target member read preserves legacy `already_held` / read-failure
  fallback behavior. Before a PUT, authoritative bot-member and guild-role reads
  must succeed; managed roles, `@everyone`, and targets at or above the bot's
  highest position refuse. Equal positions conservatively refuse.
- `GuildAddMemberRequest` holds only the validated user ID. The OAuth token is a
  separate borrowed function argument. Twilight builds the PUT body; no builder,
  body, provider response, or transport error is formatted into a log or error.
- Only 2xx responses succeed. 201 returns `added`; other 2xx statuses return
  `already_member`. Role results are `assigned` and `already_held`. Redirects
  are never followed or accepted as success; OAuth/bot credentials are never
  forwarded to a `Location` target.
- `MemberOutcome::success_body` preserves legacy JSON insertion order:
  `ok`, `result: { outcome }`, `request_id`.
- Member addition has the legacy 1500 ms timeout; each role exchange has 2000 ms.
  There are no implicit mutation retries. Internal error mapping uses the
  `Retry-After` header (rounded up, minimum one second), not moderation's
  response-body retry parser. 403/404 map to `discord_rejected`, 5xx to
  `discord_unavailable`, and aborted requests to `upstream_timeout`.

Sources:
- https://docs.discord.com/developers/resources/guild#add-guild-member
- https://docs.discord.com/developers/resources/guild#add-guild-member-role
- https://docs.discord.com/developers/topics/permissions#permission-hierarchy
- Legacy `src/internal/actions.ts:301-346`, `discordActions.ts:111-151,262-285`.

## Durable integration (`two-bot-discord/db` feature)

`execute_stored_member` takes exact authenticated payload bytes, a stable logical
caller, an idempotency key, and server configuration. It validates the payload
and IDs before claiming. The configured guild/role map must not be supplied by
the website. Addition is disabled unless the evaluated rollout flag is true.

The receiver must authenticate, perform the durable nonce burn, and apply the
existing action gates and token buckets **before** this call. This module is not
an HTTP endpoint or an authorization bypass. See `internal-action-store.md`.

Only `InternalClaim::Claimed` may make REST calls. Success is returned only after
`finish` commits. The store's typed scalar `affected` is 1 for added/assigned and
0 for already-member/already-held, with no resource ID. Action plus that scalar
reconstructs the exact success result on replay. Caller/key/payload are hashes,
and audit subjects contain only validated IDs.

Definitive Discord refusals terminalize as `discord_rejected`. Replay preserves
the code, HTTP status, and retryability but uses a generic rejection message;
provider/status-detail text is not in the store. The website branches on codes,
not English messages. Reconciliation may also record `Malformed` (400),
`ActionNotAllowed` (403), or `NoEffect` (502). Replay maps the first two to their
same-named legacy codes. `NoEffect` uses legacy `discord_unavailable`/502
(retryable), with safe log reason `no_effect`, because the legacy envelope has
no `no_effect` code. The recorded intent remains terminal: repeating the same
key returns the cached failure without REST, even when its wire code is
retryable. Only invalid response records report `store_unavailable`.

Timeouts, transport failures, 429/5xx, redirects, and unreadable policy reads
conservatively retain an unknown fence. A repeated key returns `in_progress` without sending
again. This follows the existing store's no-automatic-reclaim contract; it is
not a retry queue. The receiver/reconciliation owner must resolve unknown
outcomes before another execution is authorized. A non-durable caller can
explicitly retry after `Retry-After`, but must not use that seam to bypass a
stored fence. A post-effect storage failure also retains execution ownership.

## Tests

`internal_member` uses only the loopback REST double and fixture credentials.
It asserts routes/bodies, all four byte-exact success responses, hierarchy and
allowlist refusal, 403/404, caller-controlled 429 retry, and token-free tracing
on success, error and timeout.

`internal_member_store` runs explicitly against `agent-testdb:5432/agent_test`,
user `agent_test`, empty password, using the **same guard** as the existing core
store suite. It creates and removes only its own generated schema. No other DB
URL or credential fallback is allowed. CI explicitly invokes the ignored tests.

```sh
cargo test -p two-bot-discord --locked --test internal_member
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test \
  cargo test -p two-bot-discord --features db --locked \
  --test internal_member_store -- --ignored
```
